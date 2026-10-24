//! The built pbfhogg executable and the flags brokkr negotiates with it.
//!
//! pbfhogg refuses to write an output path that already exists unless it is
//! given `--overwrite`, and brokkr reuses its scratch outputs on every
//! iteration. Old pbfhogg commits - the ones `--commit` baselines build -
//! predate the flag, and clap rejects an argument it does not know. So the
//! flag is passed only when the executable about to run accepts it, which is
//! asked of that executable, never inferred from a commit range.
//!
//! [`PbfhoggBinary`] is the one place that knows about the flag. Every pbfhogg
//! spawn brokkr makes is an [`Invocation`] it produced: the caller states
//! whether the argv writes an output brokkr owns ([`Writes`], or the typed
//! command's own [`CommandArgv`]), and the handle appends `--overwrite` when
//! the output is owned and the subcommand accepts it. No caller passes the
//! flag itself.
//!
//! # The probe
//!
//! Support is decided per subcommand by asking pbfhogg's clap parser rather
//! than reading its help text, which no grammar over rendered help can do
//! reliably (wrapped and next-line descriptions, custom headings, prose):
//!
//! - `SUB --help` must exit 0: the subcommand exists and help works.
//! - `SUB --hel --help` must exit 2. `--hel` is a strict prefix of `--help`,
//!   so clap rejects it as unknown only when long-option inference is off and
//!   an unknown flag in that position is not swallowed as a positional value
//!   - the two prerequisites for the next probe meaning what it says.
//! - `SUB --overwrite --help`: 0 is supported, 2 is not. Clap parses in order
//!   and errors on an unknown argument before the help action fires or
//!   required arguments are checked.
//!
//! Anything else - another exit code, a signal, a spawn failure - is an error
//! naming the probe, never a cached "unsupported". The contract relied on is
//! clap's: help exits 0, a usage error exits 2.
//!
//! # Caching
//!
//! Answers are persisted in `.brokkr/pbfhogg-capabilities`, keyed by probe
//! protocol, the executable's XXH128 and the subcommand. Different bytes
//! never reuse an answer; identical bytes rebuilt for a later baseline skip
//! the probe. Only conclusive answers are recorded.
//!
//! # Lifetime
//!
//! A handle runs the bytes it probed for as long as it exists. It holds the
//! brokkr lock (a shared guard on this process's hold), so no other
//! cooperating brokkr process can build; and it is registered with
//! [`build::executable_guard`](crate::build::executable_guard), which refuses
//! any cargo spawn through the build seam while a handle is live and refuses
//! a handle while a build runs. The registration is taken before the handle
//! reads its executable, and both are released by RAII. An invocation
//! borrows its handle, so it cannot outlive that protection. Invocation
//! construction still re-stats the path as a backstop against a change made
//! outside cooperating brokkr commands.
//!
//! # Boundary
//!
//! Code in `src/pbfhogg` never holds the path of a pbfhogg build brokkr made
//! except through this handle. The runner's [`Executable`] is guarded by
//! types: its path is readable only with a [`HandleKey`], which only this
//! module can make, and `Invocation::built` refuses a pbfhogg executable.
//! The other seams that hand out built paths (the build, harness-build and
//! check modules, cargo spawned directly, a session's binary) are kept out of
//! the rest of `src/pbfhogg` by two one-owner rules in `brokkr.toml`, which
//! assume rustfmt layout and do not see identifiers a macro assembles.
//! Nothing here exposes the path. Starting pbfhogg by
//! bare name from `PATH`, deriving a path from data (an artifact stream, a
//! recorded `binary_path`, `target_dir` joined by hand), or calling another
//! command whole are outside that claim.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::build::executable_guard::{self, HandleToken};
use crate::error::DevError;
use crate::measure::{Executable, Invocation};
use crate::output;
use crate::pbfhogg::commands::CommandArgv;

/// pbfhogg's opt-in for replacing an existing output path.
pub(crate) const OVERWRITE_FLAG: &str = "--overwrite";

/// Bumped whenever the probe's decision rule changes, so answers recorded by
/// an older rule are never consumed.
const PROBE_PROTOCOL: u32 = 1;

/// Most recent answers kept in the store; older ones are dropped on save.
const STORE_CAP: usize = 512;

/// Whether a hand-built pbfhogg argv writes an output path brokkr owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Writes {
    /// Reads only, or writes nothing brokkr reuses.
    Nothing,
    /// Writes a scratch, cache or staged output brokkr reuses or retries, so
    /// pbfhogg must be allowed to replace it.
    OwnedOutput,
}

/// The capability to read an [`Executable`]'s path. Its field is private to
/// this module, so no other code can make one: the runner's executable
/// becomes a path only inside a `PbfhoggBinary`.
pub(crate) struct HandleKey(());

/// A built pbfhogg executable plus the per-subcommand capability answers
/// negotiated with it. See the module docs for the lifetime guarantee.
pub(crate) struct PbfhoggBinary {
    path: PathBuf,
    identity: FileIdentity,
    digest: String,
    project_root: PathBuf,
    answers: RefCell<BTreeMap<String, bool>>,
    /// Keeps the brokkr lock held for the handle's life (`None` only for
    /// test fixtures, which run without the lock).
    _hold: Option<crate::lockfile::LockGuard>,
    /// This handle's registration against concurrent builds.
    _registered: HandleToken,
}

impl std::fmt::Debug for PbfhoggBinary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PbfhoggBinary")
            .field("digest", &self.digest)
            .finish_non_exhaustive()
    }
}

impl PbfhoggBinary {
    /// Build pbfhogg with `config` from `build_root` and wrap the result.
    /// `project_root` holds the `.brokkr` stores (the hash cache and the
    /// capability answers) and is the probe's working directory.
    pub(crate) fn build(
        config: &crate::build::BuildConfig,
        build_root: &Path,
        project_root: &Path,
    ) -> Result<(Self, crate::measure::BuildIdentity), DevError> {
        let built = crate::build::cargo_build(config, build_root)?;
        let identity = crate::measure::BuildIdentity::of(&built);
        Ok((Self::locked(&built.path, project_root)?, identity))
    }

    /// Wrap the executable a measurement session built.
    pub(crate) fn from_session(
        session: &crate::context::MeasurementSession,
        project_root: &Path,
    ) -> Result<Self, DevError> {
        Self::locked(session.binary()?, project_root)
    }

    /// Wrap the executable the measure runner built for a plan.
    pub(crate) fn from_executable(
        executable: Executable<'_>,
        project_root: &Path,
    ) -> Result<Self, DevError> {
        Self::locked(executable.raw_path(HandleKey(())), project_root)
    }

    fn locked(path: &Path, project_root: &Path) -> Result<Self, DevError> {
        let hold = crate::lockfile::current_hold().ok_or_else(|| {
            DevError::Lock(
                "a pbfhogg handle needs the brokkr lock held for its whole life - \
                 this is a brokkr bug: the command must take the lock first"
                    .into(),
            )
        })?;
        Self::open(path, project_root, Some(hold))
    }

    /// A handle without the lock, for fixture executables in tests. It is
    /// still registered, so tests exercise the build exclusion.
    #[cfg(test)]
    pub(crate) fn fixture(path: &Path, project_root: &Path) -> Result<Self, DevError> {
        Self::open(path, project_root, None)
    }

    fn open(
        path: &Path,
        project_root: &Path,
        hold: Option<crate::lockfile::LockGuard>,
    ) -> Result<Self, DevError> {
        // Register before reading the executable, so no build can replace it
        // between that read and the handle's last use. A failure below drops
        // the registration with it.
        let registered = executable_guard::reserve_handle()?;
        let meta = std::fs::metadata(path).map_err(|e| {
            DevError::Build(format!(
                "pbfhogg executable {} is unreadable: {e}",
                path.display()
            ))
        })?;
        let identity = FileIdentity::of(&meta);
        let digest = match crate::preflight::digest::cached_path_digest(path, project_root)? {
            crate::preflight::digest::PathDigest::File(digest) => digest.to_string(),
            crate::preflight::digest::PathDigest::Tree(_) => {
                return Err(DevError::Build(format!(
                    "pbfhogg executable {} is a directory",
                    path.display()
                )));
            }
        };
        let answers = CapabilityStore::open(project_root).answers_for(&digest);
        Ok(Self {
            path: path.to_path_buf(),
            identity,
            digest,
            project_root: project_root.to_path_buf(),
            answers: RefCell::new(answers),
            _hold: hold,
            _registered: registered,
        })
    }

    /// The executable for this module's own messages; never handed out.
    fn label(&self) -> String {
        self.path.display().to_string()
    }

    /// Resolve a hand-built argv. `writes` is the caller's declaration.
    pub(crate) fn invocation(
        &self,
        args: Vec<String>,
        writes: Writes,
    ) -> Result<Invocation<'_>, DevError> {
        self.resolve(args, writes == Writes::OwnedOutput)
    }

    /// [`invocation`](Self::invocation) for a borrowed `&str` argv.
    pub(crate) fn invocation_strs(
        &self,
        args: &[&str],
        writes: Writes,
    ) -> Result<Invocation<'_>, DevError> {
        self.invocation(args.iter().map(|&a| a.to_owned()).collect(), writes)
    }

    /// Resolve a typed command's argv. Whether it negotiates the flag was
    /// decided by the same match arm that built the arguments.
    pub(crate) fn command_invocation(&self, argv: CommandArgv) -> Result<Invocation<'_>, DevError> {
        let negotiate = argv.negotiates_overwrite();
        self.resolve(argv.into_args(), negotiate)
    }

    fn resolve(&self, mut args: Vec<String>, negotiate: bool) -> Result<Invocation<'_>, DevError> {
        let Some(subcommand) = args.first().cloned() else {
            return Err(DevError::Internal(
                "empty pbfhogg argv has no subcommand".into(),
            ));
        };
        self.check_identity()?;
        if negotiate && self.supports_overwrite(&subcommand)? {
            args.push(OVERWRITE_FLAG.into());
        }
        Ok(Invocation::new(&self.path, args))
    }

    /// Refuse an executable replaced since this handle was made: its cached
    /// answers describe bytes that are no longer the ones that would run.
    /// Builds cannot do this while the handle lives; this catches a change
    /// made outside cooperating brokkr commands.
    fn check_identity(&self) -> Result<(), DevError> {
        let now = std::fs::metadata(&self.path)
            .map(|meta| FileIdentity::of(&meta))
            .map_err(|e| {
                DevError::Internal(format!(
                    "pbfhogg executable {} vanished under a live handle: {e}",
                    self.path.display()
                ))
            })?;
        if now == self.identity {
            Ok(())
        } else {
            Err(DevError::Internal(format!(
                "pbfhogg executable {} was replaced under a live handle - \
                 something outside brokkr's build seam rewrote it",
                self.path.display()
            )))
        }
    }

    fn supports_overwrite(&self, subcommand: &str) -> Result<bool, DevError> {
        if let Some(&known) = self.answers.borrow().get(subcommand) {
            return Ok(known);
        }
        let supported = self.probe(subcommand)?;
        self.answers
            .borrow_mut()
            .insert(subcommand.to_owned(), supported);
        CapabilityStore::open(&self.project_root).record(&self.digest, subcommand, supported);
        Ok(supported)
    }

    fn probe(&self, subcommand: &str) -> Result<bool, DevError> {
        let program = self.label();
        let run = |extra: &[&str]| -> Result<Option<i32>, DevError> {
            let mut args = vec![subcommand];
            args.extend_from_slice(extra);
            match output::run_captured(&program, &args, &self.project_root) {
                Ok(captured) => Ok(captured.code()),
                Err(DevError::Interrupted) => Err(DevError::Interrupted),
                Err(e) => Err(DevError::Build(format!(
                    "cannot tell whether `{program} {subcommand}` accepts {OVERWRITE_FLAG}: \
                     probe `{}` did not run: {e}",
                    args.join(" ")
                ))),
            }
        };
        let help = run(&["--help"])?;
        let control = run(&["--hel", "--help"])?;
        let overwrite = run(&[OVERWRITE_FLAG, "--help"])?;
        decide(help, control, overwrite).map_err(|why| {
            DevError::Build(format!(
                "cannot tell whether `{program} {subcommand}` accepts {OVERWRITE_FLAG}: {why}"
            ))
        })
    }
}

/// The probe's decision from the three exit codes (`None` is a signal).
fn decide(help: Option<i32>, control: Option<i32>, overwrite: Option<i32>) -> Result<bool, String> {
    if help != Some(0) {
        return Err(format!(
            "`--help` exited {} instead of 0, so the subcommand or its help is broken",
            show_code(help)
        ));
    }
    if control != Some(2) {
        return Err(format!(
            "the unknown flag `--hel` exited {} instead of 2, so this parser does not \
             reject unknown flags exactly (long-option inference, or a positional that \
             takes hyphen values)",
            show_code(control)
        ));
    }
    match overwrite {
        Some(0) => Ok(true),
        Some(2) => Ok(false),
        other => Err(format!(
            "`{OVERWRITE_FLAG} --help` exited {}, expected 0 or 2",
            show_code(other)
        )),
    }
}

fn show_code(code: Option<i32>) -> String {
    code.map_or_else(|| "on a signal".to_owned(), |c| c.to_string())
}

/// What identifies the file at a path closely enough to notice it being
/// replaced: cargo publishes a rebuilt executable as a new file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
    len: u64,
    mtime_ns: i128,
    ctime_ns: i128,
}

impl FileIdentity {
    fn of(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        let ns = |secs: i64, nsec: i64| i128::from(secs) * 1_000_000_000 + i128::from(nsec);
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            len: meta.len(),
            mtime_ns: ns(meta.mtime(), meta.mtime_nsec()),
            ctime_ns: ns(meta.ctime(), meta.ctime_nsec()),
        }
    }
}

/// `.brokkr/pbfhogg-capabilities`: one answer per line,
/// `<protocol>\t<xxh128>\t<subcommand>\t<0|1>`, oldest first. A cache, never
/// a source of truth: unreadable or malformed lines are dropped, and a failed
/// save only costs a re-probe.
struct CapabilityStore {
    file: PathBuf,
}

impl CapabilityStore {
    fn open(project_root: &Path) -> Self {
        Self {
            file: crate::clean::layout::ProjectState::new(project_root).pbfhogg_capabilities_file(),
        }
    }

    fn read(&self) -> Vec<(String, String, bool)> {
        std::fs::read_to_string(&self.file)
            .unwrap_or_default()
            .lines()
            .filter_map(parse_store_line)
            .collect()
    }

    fn answers_for(&self, digest: &str) -> BTreeMap<String, bool> {
        self.read()
            .into_iter()
            .filter(|(d, _, _)| d == digest)
            .map(|(_, subcommand, supported)| (subcommand, supported))
            .collect()
    }

    fn record(&self, digest: &str, subcommand: &str, supported: bool) {
        let mut entries: Vec<_> = self
            .read()
            .into_iter()
            .filter(|(d, s, _)| !(d == digest && s == subcommand))
            .collect();
        entries.push((digest.to_owned(), subcommand.to_owned(), supported));
        let skip = entries.len().saturating_sub(STORE_CAP);
        let mut text = String::new();
        for (d, s, supported) in entries.iter().skip(skip) {
            text.push_str(&format!(
                "{PROBE_PROTOCOL}\t{d}\t{s}\t{}\n",
                u8::from(*supported)
            ));
        }
        let saved = self
            .file
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| crate::atomic_write::replace(&self.file, text.as_bytes()));
        if let Err(e) = saved {
            output::warn(&format!(
                "could not record pbfhogg capability in {}: {e}",
                self.file.display()
            ));
        }
    }
}

/// One store line under the current protocol; `None` for anything else.
fn parse_store_line(line: &str) -> Option<(String, String, bool)> {
    let mut parts = line.split('\t');
    let protocol: u32 = parts.next()?.parse().ok()?;
    let digest = parts.next()?;
    let subcommand = parts.next()?;
    let supported = match parts.next()? {
        "0" => false,
        "1" => true,
        _ => return None,
    };
    if protocol != PROBE_PROTOCOL
        || parts.next().is_some()
        || digest.is_empty()
        || subcommand.is_empty()
    {
        return None;
    }
    Some((digest.to_owned(), subcommand.to_owned(), supported))
}

#[cfg(test)]
#[path = "binary_tests.rs"]
pub(crate) mod tests;
