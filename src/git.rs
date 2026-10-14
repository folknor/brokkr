use std::fmt;
use std::path::Path;
use std::str::FromStr;

use crate::error::DevError;

mod tree_state;
pub(crate) use tree_state::TreeState;

/// Width, in hex digits, of every abbreviated commit hash brokkr stores or
/// names something by: results rows, `--commit` worktree names, bench
/// baselines, tilegen archives. Matches the crate's own `build.rs` stamp.
///
/// Fixed rather than `git rev-parse --short`'s default, which grows with the
/// repo (7, then 8, ...): the same commit got a new worktree/baseline/archive
/// name once the repo crossed a threshold, orphaning the old one, and a hash
/// copied at one width failed to find artefacts named at another. Truncation
/// of the full hash, not `--short=9`, because `--short=N` still lengthens an
/// ambiguous abbreviation - the name must be a pure function of the commit.
pub const SHORT_HASH_LEN: usize = 9;

/// A full Git commit object id. Only SHA-1 (40 hex digits) and SHA-256
/// (64 hex digits) object ids are accepted.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Commit(String);

impl Commit {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The single abbreviation rule used for commit identity throughout
    /// brokkr.
    pub fn short(&self) -> ShortCommit {
        ShortCommit(self.0[..SHORT_HASH_LEN].to_owned())
    }
}

impl FromStr for Commit {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if !matches!(value.len(), 40 | 64) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!(
                "commit id must be 40 or 64 hexadecimal characters; got {value:?}"
            ));
        }
        Ok(Self(value.to_ascii_lowercase()))
    }
}

impl fmt::Debug for Commit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Commit({})", self.0)
    }
}

impl fmt::Display for Commit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A commit abbreviation used for persistent keys and display. Stored legacy
/// prefixes may be shorter than the current fixed width, and a value read
/// from an old store may be non-hex legacy text kept verbatim
/// ([`ShortCommit::from_stored_lossy`]); new abbreviations always come from
/// [`Commit::short`] and have [`SHORT_HASH_LEN`] digits.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ShortCommit(String);

impl ShortCommit {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Convert a persisted commit field written by older brokkr versions.
    /// New rows are abbreviated, while some external stores may retain the
    /// full object id.
    pub fn from_stored(value: &str) -> Result<Self, String> {
        if matches!(value.len(), 40 | 64) {
            value.parse::<Commit>().map(|commit| commit.short())
        } else {
            value.parse()
        }
    }

    /// The read policy for stored rows: a recognised commit is normalised as
    /// [`Self::from_stored`] does, and anything else (`""`, `unknown`,
    /// `dirty`, other legacy text) is kept verbatim rather than refused, so a
    /// store written by an older brokkr always stays readable. New values
    /// never take this path; they come from [`Commit::short`].
    pub fn from_stored_lossy(value: &str) -> Self {
        Self::from_stored(value).unwrap_or_else(|_| Self(value.to_owned()))
    }
}

impl FromStr for ShortCommit {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty()
            || value.len() > SHORT_HASH_LEN
            || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(format!(
                "short commit id must contain 1 to {SHORT_HASH_LEN} hexadecimal characters; got {value:?}"
            ));
        }
        Ok(Self(value.to_ascii_lowercase()))
    }
}

impl fmt::Debug for ShortCommit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ShortCommit({})", self.0)
    }
}

impl fmt::Display for ShortCommit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Resolve `rev` to the commit it names, peeling tags. The single entry point
/// for turning a user-typed revision into brokkr's commit identity, so a hash
/// pasted at any width names the same worktree, baseline and archive.
pub fn resolve_commit(repo: &Path, rev: &str) -> Result<Commit, DevError> {
    let spec = format!("{rev}^{{commit}}");
    let output = crate::output::child::command_output(
        crate::process::spawn::ChildPolicy::plain_command("git")
            .args(["rev-parse", "--verify", "--quiet", &spec])
            .current_dir(repo),
    )
    .map_err(|error| DevError::Spawn {
        program: "git".to_owned(),
        error,
    })?;
    if !output.status.success() {
        return Err(DevError::Config(format!(
            "{rev:?} does not name a commit in {}",
            repo.display()
        )));
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .map_err(|error: String| {
            DevError::Config(format!(
                "git returned an invalid commit id for {rev:?}: {error}"
            ))
        })
}

/// Whether `repo` has uncommitted changes under the run-record policy.
/// `None` means git could not answer.
pub fn has_uncommitted(repo: &Path) -> Option<bool> {
    Some(TreeState::read(repo)?.for_record())
}

/// Structured git state for the benchmark harness.
pub struct GitInfo {
    /// `HEAD`'s fixed-width abbreviation.
    pub commit: ShortCommit,
    /// First line of the commit message.
    pub subject: String,
    /// True when the tree is clean under the measurement policy.
    pub is_clean: bool,
}

/// Collect git information from the working directory. The files the
/// governing `brokkr.toml` `include`s are exempt from the measurement policy
/// exactly as `brokkr.toml` itself is: brokkr writes dataset registrations
/// into whichever file holds the dataset.
pub fn collect(workspace_root: &Path) -> Result<GitInfo, DevError> {
    // This convenience path remains for callers that only have a repository
    // path. Callers with startup `ConfigSources` must pass `sources.included()`
    // to `collect_with_config_paths` so provenance is not recomposed here.
    collect_with_config_paths(workspace_root, &included_config_files(workspace_root))
}

/// The composed config's included files. A config that does not compose adds
/// nothing: the command that loaded it has already failed, or this is not a
/// brokkr project at all.
pub(crate) fn included_config_files(workspace_root: &Path) -> Vec<std::path::PathBuf> {
    let Some(dir) = crate::project::find_config_dir(workspace_root) else {
        return Vec::new();
    };
    match crate::config::compose(&dir.join("brokkr.toml")) {
        Ok((_, sources)) => sources.included().to_vec(),
        Err(_) => Vec::new(),
    }
}

/// Collect git information using the already-resolved paths of included
/// brokkr configuration files. Paths are absolute as recorded by config
/// composition. Callers with a `Detection` should pass its composition
/// sources rather than asking git to parse config again.
pub(crate) fn collect_with_config_paths(
    workspace_root: &Path,
    included_config_files: &[std::path::PathBuf],
) -> Result<GitInfo, DevError> {
    let commit = read_commit_hash(workspace_root)?.short();
    let subject = read_commit_subject(workspace_root)?;
    // Git failures keep the old conservative behavior: unknown is not clean.
    let is_clean = TreeState::read_for_measurement(workspace_root, included_config_files)
        .is_some_and(|state| !state.for_measurement());

    Ok(GitInfo {
        commit,
        subject,
        is_clean,
    })
}

fn read_commit_hash(workspace_root: &Path) -> Result<Commit, DevError> {
    let output = crate::output::child::command_output(
        crate::process::spawn::ChildPolicy::plain_command("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(workspace_root),
    )
    .map_err(|error| DevError::Spawn {
        program: "git".to_owned(),
        error,
    })?;

    if !output.status.success() {
        return Err(DevError::Subprocess {
            program: "git".to_owned(),
            ending: crate::error::ProcessEnding::from_status(&output.status),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }

    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .map_err(|error: String| {
            DevError::Config(format!("git returned an invalid HEAD commit id: {error}"))
        })
}

fn read_commit_subject(workspace_root: &Path) -> Result<String, DevError> {
    let output = crate::output::child::command_output(
        crate::process::spawn::ChildPolicy::plain_command("git")
            .args(["log", "-1", "--format=%s"])
            .current_dir(workspace_root),
    )
    .map_err(|error| DevError::Spawn {
        program: "git".to_owned(),
        error,
    })?;

    if !output.status.success() {
        return Err(DevError::Subprocess {
            program: "git".to_owned(),
            ending: crate::error::ProcessEnding::from_status(&output.status),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
