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
//! # The private directory
//!
//! A pbfhogg child killed mid-write (`brokkr kill`, a timeout, SIGKILL) leaves
//! its staging file and run scratch behind, and pbfhogg reports such leftovers
//! but never removes them. So every argv that writes an owned output also gets
//! a directory brokkr owns: `.brokkr-pbfhogg` in the directory the output
//! lands in (the parent of `-o`, `--output-dir` or `--config`), passed as
//! `--staging-dir` and `--tmp-dir` where the subcommand accepts them. Anything
//! disposable a killed child leaves is then in there, and brokkr removes the
//! directory whole when the handle drops. pbfhogg requires a staging dir on the
//! output's mount and not containing the output's directory, and for
//! `build-geocode-index` not inside `--output-dir`: a directory beside the
//! output meets all three. The name is fixed, not per child, so a recorded
//! `cli_args` does not change between runs. That is safe because the brokkr
//! lock keeps every other brokkr's children out: on the first claim in this
//! process, whatever the directory holds is a killed run's and is removed
//! first. Old pbfhogg builds that accept neither flag get neither, and an argv
//! that already names one of them is left alone. A backup pbfhogg keeps beside
//! a replaced output is outside the directory and is never removed.
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
//!   required arguments are checked. `--staging-dir=. --help` and
//!   `--tmp-dir=. --help` are asked the same way, with a value attached so the
//!   flag cannot take `--help` as its value; one run of the two controls
//!   serves all three.
//!
//! Anything else - another exit code, a signal, a spawn failure - is an error
//! naming the probe, never a cached "unsupported". The contract relied on is
//! clap's: help exits 0, a usage error exits 2.
//!
//! # Caching
//!
//! Answers are persisted in `.brokkr/pbfhogg-capabilities`, keyed by probe
//! protocol, the executable's XXH128 and the subcommand (`sort` for
//! `--overwrite`, as before the other flags existed, and `sort --staging-dir`
//! for the others). Different bytes
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

/// pbfhogg's directory for an output's staging entry.
pub(crate) const STAGING_DIR_FLAG: &str = "--staging-dir";

/// pbfhogg's directory for a run's scratch.
pub(crate) const TMP_DIR_FLAG: &str = "--tmp-dir";

/// A flag whose support the handle negotiates per subcommand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flag {
    Overwrite,
    StagingDir,
    TmpDir,
}

impl Flag {
    const ALL: [Self; 3] = [Self::Overwrite, Self::StagingDir, Self::TmpDir];

    fn name(self) -> &'static str {
        match self {
            Self::Overwrite => OVERWRITE_FLAG,
            Self::StagingDir => STAGING_DIR_FLAG,
            Self::TmpDir => TMP_DIR_FLAG,
        }
    }

    /// The argument the probe passes before `--help`.
    fn probe_arg(self) -> String {
        match self {
            Self::Overwrite => OVERWRITE_FLAG.to_owned(),
            Self::StagingDir | Self::TmpDir => format!("{}=.", self.name()),
        }
    }

    /// The store and answer key. `--overwrite` keeps the bare subcommand, the
    /// key it had before the other flags were negotiated.
    fn key(self, subcommand: &str) -> String {
        match self {
            Self::Overwrite => subcommand.to_owned(),
            Self::StagingDir | Self::TmpDir => format!("{subcommand} {}", self.name()),
        }
    }
}

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
    /// The private directories this handle gave its children, removed when it
    /// drops. Declared before the hold, so the lock is still held then.
    private_dirs: RefCell<Vec<private_dir::Claim>>,
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
            private_dirs: RefCell::new(Vec::new()),
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
        let owned = writes == Writes::OwnedOutput;
        self.resolve(args, owned, owned)
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
        let owned = argv.writes_output();
        self.resolve(argv.into_args(), negotiate, owned)
    }

    /// `negotiate`: pass `--overwrite` where accepted. `owned`: the argv writes
    /// an owned output, so it gets the private directory (module docs).
    fn resolve(
        &self,
        mut args: Vec<String>,
        negotiate: bool,
        owned: bool,
    ) -> Result<Invocation<'_>, DevError> {
        let Some(subcommand) = args.first().cloned() else {
            return Err(DevError::Internal(
                "empty pbfhogg argv has no subcommand".into(),
            ));
        };
        self.check_identity()?;
        if negotiate && self.supports(&subcommand, Flag::Overwrite)? {
            args.push(OVERWRITE_FLAG.into());
        }
        if owned {
            self.give_private_dir(&subcommand, &mut args)?;
        }
        Ok(Invocation::new(&self.path, args))
    }

    /// Append `--staging-dir` and `--tmp-dir` naming the private directory
    /// beside the argv's output, for the flags the subcommand accepts.
    fn give_private_dir(&self, subcommand: &str, args: &mut Vec<String>) -> Result<(), DevError> {
        let names_one = |arg: &String| {
            [STAGING_DIR_FLAG, TMP_DIR_FLAG]
                .iter()
                .any(|flag| arg == flag || arg.strip_prefix(flag).is_some_and(|rest| rest.starts_with('=')))
        };
        if args.iter().any(names_one) {
            return Ok(());
        }
        let Some(anchor) = output_anchor(args, &self.project_root) else {
            return Ok(());
        };
        let staging = self.supports(subcommand, Flag::StagingDir)?;
        let tmp = self.supports(subcommand, Flag::TmpDir)?;
        if !staging && !tmp {
            return Ok(());
        }
        let dir = crate::clean::layout::PbfhoggLayout::child_dir(&anchor);
        {
            let mut claims = self.private_dirs.borrow_mut();
            if !claims.iter().any(|claim| claim.dir() == dir) {
                claims.push(private_dir::Claim::take(dir.clone())?);
            }
        }
        let dir = crate::utf8_path::path_string(&dir)?;
        for (accepted, flag) in [(staging, STAGING_DIR_FLAG), (tmp, TMP_DIR_FLAG)] {
            if accepted {
                args.push(flag.into());
                args.push(dir.clone());
            }
        }
        Ok(())
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

    /// Whether `subcommand` accepts `flag`. A missing answer probes every
    /// negotiated flag at once, sharing the two controls, and records them all.
    fn supports(&self, subcommand: &str, flag: Flag) -> Result<bool, DevError> {
        if let Some(&known) = self.answers.borrow().get(&flag.key(subcommand)) {
            return Ok(known);
        }
        let answers = self.probe(subcommand)?;
        let store = CapabilityStore::open(&self.project_root);
        let mut cached = self.answers.borrow_mut();
        for (probed, supported) in &answers {
            let key = probed.key(subcommand);
            store.record(&self.digest, &key, *supported);
            cached.insert(key, *supported);
        }
        Ok(answers.iter().any(|(probed, supported)| *probed == flag && *supported))
    }

    fn probe(&self, subcommand: &str) -> Result<Vec<(Flag, bool)>, DevError> {
        let program = self.label();
        let flags = Flag::ALL.map(Flag::name).join(", ");
        let run = |extra: &[&str]| -> Result<Option<i32>, DevError> {
            let mut args = vec![subcommand];
            args.extend_from_slice(extra);
            match output::run_captured(&program, &args, &self.project_root) {
                Ok(captured) => Ok(captured.code()),
                Err(DevError::Interrupted) => Err(DevError::Interrupted),
                Err(e) => Err(DevError::Build(format!(
                    "cannot tell whether `{program} {subcommand}` accepts {flags}: \
                     probe `{}` did not run: {e}",
                    args.join(" ")
                ))),
            }
        };
        let help = run(&["--help"])?;
        let control = run(&["--hel", "--help"])?;
        Flag::ALL
            .into_iter()
            .map(|flag| {
                let code = run(&[&flag.probe_arg(), "--help"])?;
                let supported = decide(help, control, code, flag.name()).map_err(|why| {
                    DevError::Build(format!(
                        "cannot tell whether `{program} {subcommand}` accepts {}: {why}",
                        flag.name()
                    ))
                })?;
                Ok((flag, supported))
            })
            .collect()
    }
}

/// Where an argv's output lands: the directory holding the path given to
/// `-o`/`--output` (a file or a directory output), `--output-dir` (the
/// geocode index, which publication replaces whole) or `--config` (whose
/// outputs brokkr writes beside it). Relative paths are against `cwd`, the
/// directory pbfhogg runs in. `None` for an argv naming none, or naming a
/// device such as `/dev/null`.
fn output_anchor(args: &[String], cwd: &Path) -> Option<PathBuf> {
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let (flag, attached) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value)),
            _ => (arg.as_str(), None),
        };
        if !matches!(flag, "-o" | "--output" | "--output-dir" | "--config") {
            continue;
        }
        let value = match attached {
            Some(value) => value,
            None => rest.next()?.as_str(),
        };
        let path = Path::new(value);
        if path.starts_with("/dev") {
            return None;
        }
        let path = if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) };
        return path.parent().map(Path::to_path_buf);
    }
    None
}

/// The process-wide claims on private directories (module docs). Several
/// handles in one process may hand out the same directory; it is emptied on
/// its first claim and removed when its last claim drops.
mod private_dir {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, PoisonError};

    use crate::error::DevError;
    use crate::output;

    static CLAIMS: Mutex<BTreeMap<PathBuf, usize>> = Mutex::new(BTreeMap::new());

    pub(super) struct Claim {
        dir: PathBuf,
    }

    impl Claim {
        /// Claim `dir`, creating it empty when this process holds no other
        /// claim on it: no child of this process uses it then, and the brokkr
        /// lock keeps every other brokkr's out, so whatever it holds is a
        /// killed run's.
        pub(super) fn take(dir: PathBuf) -> Result<Self, DevError> {
            let mut claims = CLAIMS.lock().unwrap_or_else(PoisonError::into_inner);
            let count = claims.entry(dir.clone()).or_insert(0);
            if *count == 0 {
                remove(&dir).map_err(|e| {
                    DevError::Io(std::io::Error::new(
                        e.kind(),
                        format!("cannot empty the pbfhogg private directory {}: {e}", dir.display()),
                    ))
                })?;
                std::fs::create_dir_all(&dir).map_err(|e| {
                    DevError::Io(std::io::Error::new(
                        e.kind(),
                        format!("cannot create the pbfhogg private directory {}: {e}", dir.display()),
                    ))
                })?;
            }
            *count += 1;
            Ok(Self { dir })
        }

        pub(super) fn dir(&self) -> &Path {
            &self.dir
        }
    }

    impl Drop for Claim {
        fn drop(&mut self) {
            let mut claims = CLAIMS.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(count) = claims.get_mut(&self.dir) else {
                return;
            };
            *count = count.saturating_sub(1);
            if *count > 0 {
                return;
            }
            claims.remove(&self.dir);
            if let Err(e) = remove(&self.dir) {
                output::warn(&format!(
                    "could not remove the pbfhogg private directory {}: {e}",
                    self.dir.display()
                ));
            }
        }
    }

    fn remove(dir: &Path) -> std::io::Result<()> {
        match std::fs::remove_dir_all(dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
}

/// The probe's decision from the three exit codes (`None` is a signal) for
/// the flag named `flag`.
fn decide(
    help: Option<i32>,
    control: Option<i32>,
    probed: Option<i32>,
    flag: &str,
) -> Result<bool, String> {
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
    match probed {
        Some(0) => Ok(true),
        Some(2) => Ok(false),
        other => Err(format!(
            "`{flag} --help` exited {}, expected 0 or 2",
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
