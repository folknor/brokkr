//! Durable storage for PMTiles archives produced or selected by brokkr.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::error::DevError;

/// Maximum number of archives tilegen keeps for one `(dataset, variant)`.
pub const TILEGEN_RETENTION: usize = 5;

/// The three values that identify a durable tilegen archive.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveKey {
    dataset: String,
    variant: String,
    commit: String,
}

impl ArchiveKey {
    /// Construct an archive key, refusing path components in the commit token.
    pub fn new(
        dataset: impl Into<String>,
        variant: impl Into<String>,
        commit: impl Into<String>,
    ) -> Result<Self, DevError> {
        let dataset = dataset.into();
        let variant = variant.into();
        let commit = commit.into();
        if !safe_component(&dataset) || !safe_component(&variant) || !safe_component(&commit) {
            return Err(DevError::Config(
                "PMTiles archive key fields must be non-empty filename components".into(),
            ));
        }
        if commit.contains('-') {
            return Err(DevError::Config(
                "PMTiles archive commit token cannot contain a hyphen".into(),
            ));
        }
        Ok(Self {
            dataset,
            variant,
            commit,
        })
    }

    fn group(&self) -> ArchiveGroup {
        ArchiveGroup::new(self.dataset.clone(), self.variant.clone())
    }
}

/// A configured archive group. Names are kept as supplied by configuration;
/// filenames are always constructed from these values, never split back.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ArchiveGroup {
    pub dataset: String,
    pub variant: String,
}

impl ArchiveGroup {
    pub fn new(dataset: impl Into<String>, variant: impl Into<String>) -> Self {
        Self {
            dataset: dataset.into(),
            variant: variant.into(),
        }
    }
}

/// Where a selected archive came from. Explicit `--file` paths are marked so
/// callers can preserve the distinction between store-owned and unpinned input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArchiveOrigin {
    Store(ArchiveKey),
    ExplicitFile,
}

/// A resolved archive path together with its provenance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveResolution {
    pub path: PathBuf,
    pub origin: ArchiveOrigin,
}

/// Owner of the durable PMTiles archive directory and its naming/retention
/// rules.
#[derive(Clone, Debug)]
pub struct ArchiveStore {
    dir: PathBuf,
    groups: Vec<ArchiveGroup>,
}

impl ArchiveStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            groups: Vec::new(),
        }
    }

    /// Attach the configured groups used by a bulk prune operation.
    pub fn with_groups(mut self, groups: impl IntoIterator<Item = ArchiveGroup>) -> Self {
        let mut seen = HashSet::new();
        self.groups = groups
            .into_iter()
            .filter(|group| seen.insert(group.clone()))
            .collect();
        self
    }

    /// Construct the only canonical durable archive path.
    pub fn path_for(&self, key: &ArchiveKey) -> PathBuf {
        self.dir.join(format!(
            "{}-{}-{}.pmtiles",
            key.dataset, key.variant, key.commit
        ))
    }

    /// Resolve an explicit file or a configured store key. A revision that is
    /// no longer in git may still select a stored archive by its exact token or
    /// a unique legacy short hash; an unresolvable typo gets a git error rather
    /// than a misleading "no build" message.
    pub fn resolve(
        &self,
        dataset: &str,
        variant: &str,
        revision: Option<&str>,
        file: Option<&str>,
        build_root: &Path,
    ) -> Result<ArchiveResolution, DevError> {
        if let Some(file) = file {
            let path = PathBuf::from(file);
            if !path.exists() {
                return Err(DevError::Config(format!(
                    "file not found: {}",
                    path.display()
                )));
            }
            return Ok(ArchiveResolution {
                path,
                origin: ArchiveOrigin::ExplicitFile,
            });
        }

        let key = match revision {
            Some(revision) => self.resolve_revision(dataset, variant, revision, build_root)?,
            None => {
                let commit = crate::git::collect(build_root)?.commit;
                ArchiveKey::new(dataset, variant, commit)?
            }
        };
        let path = self.path_for(&key);
        if !path.exists() {
            return Err(DevError::Config(format!(
                "no {} build for {}; run `brokkr tilegen --variant {}` first (looked for {})",
                key.variant,
                key.commit,
                key.variant,
                path.display()
            )));
        }
        Ok(ArchiveResolution {
            path,
            origin: ArchiveOrigin::Store(key),
        })
    }

    /// Move one successful tilegen output into the store and apply the
    /// store-owned per-group retention window. The protected directories are
    /// the caller's scratch and tool-temporary directories.
    pub fn admit(
        &self,
        scratch_file: &Path,
        key: &ArchiveKey,
        protected_dirs: &[&Path],
    ) -> Result<PathBuf, DevError> {
        let scratch_dir = scratch_file.parent().unwrap_or_else(|| Path::new(""));
        if self.dir.as_path() == scratch_dir
            || protected_dirs.contains(&self.dir.as_path())
        {
            return Err(DevError::Config(format!(
                "output dir {} coincides with elivagar's scratch/tmp dir; set a distinct [<host>].output in brokkr.toml. Output left in {}",
                self.dir.display(),
                scratch_file.display(),
            )));
        }

        if !scratch_file.exists() {
            return Err(DevError::Verify(format!(
                "tilegen exited 0 but wrote no output at {}",
                scratch_file.display()
            )));
        }
        std::fs::create_dir_all(&self.dir).map_err(|error| {
            DevError::Io(io::Error::new(
                error.kind(),
                format!(
                    "failed to create output dir {}: {error}; output left in {}",
                    self.dir.display(),
                    scratch_file.display(),
                ),
            ))
        })?;

        let destination = self.path_for(key);
        std::fs::rename(scratch_file, &destination).map_err(|error| {
            DevError::Io(io::Error::new(
                error.kind(),
                format!(
                    "failed to move {} to {}: {error}; output left in place",
                    scratch_file.display(),
                    destination.display(),
                ),
            ))
        })?;

        // Retention is best-effort after admission, as a pruning failure must
        // not turn a successfully stored archive into a failed tilegen run.
        let group = key.group();
        drop(self.prune_groups(std::slice::from_ref(&group), TILEGEN_RETENTION, |path| {
            std::fs::remove_file(path).is_ok()
        }));
        Ok(destination)
    }

    /// Return and remove archives beyond the newest `keep` in each configured
    /// group. The callback lets `brokkr clean` apply dry-run and accounting
    /// policy while admission uses the same ordering and membership rules.
    pub fn prune(&self, keep: usize, remove: impl FnMut(&Path) -> bool) -> io::Result<usize> {
        self.prune_groups(&self.groups, keep, remove)
    }

    fn prune_groups(
        &self,
        groups: &[ArchiveGroup],
        keep: usize,
        mut remove: impl FnMut(&Path) -> bool,
    ) -> io::Result<usize> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error),
        };
        let listing: Vec<(String, SystemTime, PathBuf)> = entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                let name = path.file_name()?.to_str()?.to_owned();
                let modified = entry
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                Some((name, modified, path))
            })
            .collect();

        let mut removed = 0;
        let mut handled = HashSet::new();
        for group in groups {
            let mut archives: Vec<(SystemTime, &Path)> = listing
                .iter()
                .filter(|(name, _, path)| {
                    Self::matches_name(name, &group.dataset, &group.variant)
                        && !handled.contains(path)
                })
                .map(|(_, modified, path)| (*modified, path.as_path()))
                .collect();
            archives.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
            let stale = archives
                .iter()
                .skip(keep)
                .map(|(_, path)| *path)
                .collect::<Vec<_>>();
            handled.extend(archives.iter().map(|(_, path)| path.to_path_buf()));
            for path in stale {
                if remove(path) {
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    pub(crate) fn matches_name(file_name: &str, dataset: &str, variant: &str) -> bool {
        file_name
            .strip_prefix(&archive_prefix(dataset, variant))
            .and_then(|rest| rest.strip_suffix(".pmtiles"))
            .is_some_and(|commit| !commit.is_empty() && !commit.contains('-'))
    }

    fn resolve_revision(
        &self,
        dataset: &str,
        variant: &str,
        revision: &str,
        build_root: &Path,
    ) -> Result<ArchiveKey, DevError> {
        match crate::git::resolve_commit(build_root, revision) {
            Ok(id) => {
                let fixed = ArchiveKey::new(dataset, variant, id.short.clone())?;
                if self.path_for(&fixed).exists() {
                    return Ok(fixed);
                }
                let commit = self
                    .legacy_archive_token(dataset, variant, &id.full)
                    .unwrap_or(id.short);
                ArchiveKey::new(dataset, variant, commit)
            }
            Err(error) => {
                if safe_component(revision) && !revision.contains('-') {
                    let exact = ArchiveKey::new(dataset, variant, revision)?;
                    if self.path_for(&exact).exists() {
                        return Ok(exact);
                    }
                    if revision.len() >= 4
                        && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
                        && let Some(commit) = self.legacy_archive_token(dataset, variant, revision)
                    {
                        return ArchiveKey::new(dataset, variant, commit);
                    }
                }
                Err(DevError::Config(format!(
                    "cannot resolve git revision '{revision}': {error}"
                )))
            }
        }
    }

    fn legacy_archive_token(&self, dataset: &str, variant: &str, full: &str) -> Option<String> {
        let prefix = archive_prefix(dataset, variant);
        let mut found: Vec<String> = std::fs::read_dir(&self.dir)
            .ok()?
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
            .filter(|name| Self::matches_name(name, dataset, variant))
            .filter_map(|name| {
                let token = name
                    .strip_prefix(&prefix)?
                    .strip_suffix(".pmtiles")?
                    .to_owned();
                (token.len() >= 4
                    && token.bytes().all(|byte| byte.is_ascii_hexdigit())
                    && full.starts_with(&token))
                .then_some(token)
            })
            .collect();
        if found.len() == 1 { found.pop() } else { None }
    }
}

fn archive_prefix(dataset: &str, variant: &str) -> String {
    format!("{dataset}-{variant}-")
}

fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains('\\')
        && !value.contains('\0')
}
