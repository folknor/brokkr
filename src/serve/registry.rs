//! The per-user registry of `brokkr serve` generations.
//!
//! One generation is three files under `~/.brokkr/serves/`:
//!
//! - `<token>.lock`: the liveness flock and the immutable [`Intent`]. It is
//!   created, flocked and written in full under `registry.lock`, before the
//!   supervisor exists, so no reader ever finds an entry whose flock nobody
//!   holds yet. The launcher holds the flock through the supervisor's spawn,
//!   the supervisor inherits the same open file description and holds it for
//!   the rest of its life, so the flock is free exactly when no supervisor of
//!   the generation is executing its protocol.
//! - `<token>.state`: the mutable [`State`] the supervisor publishes (its own
//!   identity, then the server's), replaced whole by rename so a reader sees
//!   one version or the other. Absent means the generation is starting.
//! - `registry.lock`: serialises publishing an entry and retiring one, and
//!   nothing else; it is held for a few syscalls at a time.
//!
//! An entry is retired - its files removed - only once its flock is free and
//! a carrier scan finds none of its processes left (`super::carriers`); a held
//! entry is never unlinked.

use std::io::{Read as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::lockfile::{RunToken, escape_value, unescape_value};
use crate::process::{BootId, Pid, ProcIdentity, StartTicks};

const LOCK_EXT: &str = "lock";
const STATE_EXT: &str = "state";
const REGISTRY_LOCK: &str = "registry.lock";

/// The registry directory: `~/.brokkr/serves/`, or one scratch directory per
/// test process under `cfg(test)`, so no test reads or writes the real one.
pub(super) fn dir() -> Option<PathBuf> {
    #[cfg(test)]
    {
        static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        Some(
            DIR.get_or_init(|| crate::test_scratch::scratch("serve", "registry"))
                .clone(),
        )
    }
    #[cfg(not(test))]
    {
        crate::user_dirs::serves_dir()
    }
}

/// What a generation was started as, written once before its supervisor
/// exists. Everything a reader needs to name, stop or reconcile it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Intent {
    pub(crate) token: RunToken,
    /// The brokkr project name, or `brokkr` outside a configured project.
    pub(crate) project: String,
    /// The state root (the directory holding `brokkr.toml`): with `name`, the
    /// identity a start replaces.
    pub(crate) root: PathBuf,
    pub(crate) name: String,
    /// The cargo target the program was built from.
    pub(crate) target: String,
    /// The generation directory (`.brokkr/serve/<name>.<token>/`).
    pub(crate) generation: PathBuf,
    /// The program, as linked into the generation directory.
    pub(crate) program: PathBuf,
    /// The `--with` executable, as linked into the generation directory.
    pub(crate) with: Option<PathBuf>,
    pub(crate) args: Vec<String>,
    /// The program's working directory: the build root, as for `run`.
    pub(crate) cwd: PathBuf,
    pub(crate) log: PathBuf,
    /// The server's SIGTERM grace before its group is SIGKILLed.
    pub(crate) grace: Duration,
    /// The launcher's boot and PID and time namespaces: where the recorded
    /// identities mean what they say, and the only place a carrier scan can
    /// see the generation's processes.
    pub(crate) boot_id: String,
    pub(crate) pid_ns: String,
    pub(crate) time_ns: String,
    /// Seconds since the epoch at launch.
    pub(crate) launched: u64,
}

impl Intent {
    pub(crate) fn short_id(&self) -> &str {
        short_id(&self.token)
    }

    fn render(&self) -> String {
        let path = |p: &Path| escape_value(&p.to_string_lossy());
        let mut out = String::new();
        let mut line = |key: &str, value: String| {
            out.push_str(key);
            out.push('=');
            out.push_str(&value);
            out.push('\n');
        };
        line("token", self.token.as_str().to_owned());
        line("project", escape_value(&self.project));
        line("root", path(&self.root));
        line("name", escape_value(&self.name));
        line("target", escape_value(&self.target));
        line("generation", path(&self.generation));
        line("program", path(&self.program));
        line("with", self.with.as_deref().map(path).unwrap_or_default());
        line("cwd", path(&self.cwd));
        line("log", path(&self.log));
        line("grace_ms", self.grace.as_millis().to_string());
        line("boot_id", escape_value(&self.boot_id));
        line("pid_ns", escape_value(&self.pid_ns));
        line("time_ns", escape_value(&self.time_ns));
        line("launched", self.launched.to_string());
        for arg in &self.args {
            line("arg", escape_value(arg));
        }
        out
    }

    /// Parse a record; `None` for anything incomplete. The first occurrence
    /// of a key wins, except `arg`, which repeats in order.
    fn parse(text: &str) -> Option<Self> {
        let mut fields: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
        let mut args = Vec::new();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if key == "arg" {
                args.push(unescape_value(value));
            } else {
                fields.entry(key).or_insert_with(|| unescape_value(value));
            }
        }
        let take = |key: &str| fields.get(key).cloned();
        let with = take("with").filter(|w| !w.is_empty()).map(PathBuf::from);
        Some(Self {
            token: RunToken::parse(&take("token")?)?,
            project: take("project")?,
            root: PathBuf::from(take("root")?),
            name: take("name").filter(|n| !n.is_empty())?,
            target: take("target")?,
            generation: PathBuf::from(take("generation")?),
            program: PathBuf::from(take("program")?),
            with,
            cwd: PathBuf::from(take("cwd")?),
            log: PathBuf::from(take("log")?),
            grace: Duration::from_millis(take("grace_ms")?.parse().ok()?),
            boot_id: take("boot_id")?,
            pid_ns: take("pid_ns")?,
            time_ns: take("time_ns")?,
            launched: take("launched")?.parse().ok()?,
            args,
        })
    }
}

/// The listed short form of a token.
pub(crate) fn short_id(token: &RunToken) -> &str {
    token.as_str().get(..8).unwrap_or(token.as_str())
}

/// Where a generation's supervisor stands, as it last published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// The supervisor is up and its stop handling installed; no server yet.
    Supervising,
    /// The server runs.
    Serving,
    /// The server ended; the supervisor is sweeping what is left.
    Ended,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Supervising => "supervising",
            Self::Serving => "serving",
            Self::Ended => "ended",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "supervising" => Some(Self::Supervising),
            "serving" => Some(Self::Serving),
            "ended" => Some(Self::Ended),
            _ => None,
        }
    }
}

/// What the supervisor publishes about itself and its server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct State {
    pub(crate) supervisor: ProcIdentity,
    pub(crate) server: Option<ProcIdentity>,
    pub(crate) phase: Phase,
}

impl State {
    fn render(&self) -> String {
        let pair = |id: Option<&ProcIdentity>| {
            id.map(|id| (id.pid().get().to_string(), id.start().get().to_string()))
                .unwrap_or_default()
        };
        let (sup_pid, sup_start) = pair(Some(&self.supervisor));
        let (srv_pid, srv_start) = pair(self.server.as_ref());
        format!(
            "supervisor_pid={sup_pid}\nsupervisor_start={sup_start}\nserver_pid={srv_pid}\n\
             server_start={srv_start}\nphase={}\n",
            self.phase.as_str()
        )
    }

    fn parse(text: &str) -> Option<Self> {
        let mut fields: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
        for line in text.lines() {
            if let Some((key, value)) = line.split_once('=') {
                fields.entry(key).or_insert(value);
            }
        }
        let identity = |pid: &str, start: &str| -> Option<ProcIdentity> {
            let pid = Pid::new(fields.get(pid)?.parse().ok()?)?;
            let start = StartTicks::parse(fields.get(start)?)?;
            Some(ProcIdentity::new(pid, start))
        };
        Some(Self {
            supervisor: identity("supervisor_pid", "supervisor_start")?,
            server: identity("server_pid", "server_start"),
            phase: Phase::parse(fields.get("phase")?)?,
        })
    }
}

/// An exclusive hold on `registry.lock`, waited for. Released on drop.
pub(super) struct RegistryLock {
    _file: std::fs::File,
}

impl RegistryLock {
    pub(super) fn acquire(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(REGISTRY_LOCK))?;
        lock_blocking(&file)?;
        Ok(Self { _file: file })
    }
}

/// An exclusive flock, waited for.
fn lock_blocking(file: &std::fs::File) -> std::io::Result<()> {
    loop {
        // SAFETY: a descriptor the caller owns; no pointers.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// One non-blocking exclusive flock attempt: `Ok(true)` when taken.
fn try_lock(file: &std::fs::File) -> std::io::Result<bool> {
    loop {
        // SAFETY: a descriptor the caller owns; no pointers.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EWOULDBLOCK) => return Ok(false),
            Some(libc::EINTR) => {}
            _ => return Err(error),
        }
    }
}

fn lock_path(dir: &Path, token: &RunToken) -> PathBuf {
    dir.join(format!("{}.{LOCK_EXT}", token.as_str()))
}

fn state_path(dir: &Path, token: &RunToken) -> PathBuf {
    dir.join(format!("{}.{STATE_EXT}", token.as_str()))
}

/// Publish a new generation: create its entry, take its flock and write the
/// intent, all under `registry.lock`. The returned file holds the flock; the
/// caller hands it to the supervisor and drops its own copy.
pub(super) fn publish(dir: &Path, intent: &Intent) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let _registry = RegistryLock::acquire(dir)?;
    let path = lock_path(dir, &intent.token);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .read(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC)
        .open(&path)?;
    // Waited for, not tried: a lock-free reader's probe may hold the new
    // file's flock for a moment. It reads the entry as free meanwhile, and
    // any retirement it then attempts waits for `registry.lock` and finds
    // the flock held.
    let published = lock_blocking(&file).and_then(|()| {
        file.write_all(intent.render().as_bytes())?;
        file.sync_data()
    });
    if let Err(error) = published {
        drop(std::fs::remove_file(&path));
        return Err(error);
    }
    Ok(file)
}

/// Publish the supervisor's state, replacing the previous version whole.
pub(super) fn write_state(dir: &Path, token: &RunToken, state: &State) -> std::io::Result<()> {
    let path = state_path(dir, token);
    let tmp = dir.join(format!("{}.{STATE_EXT}.tmp", token.as_str()));
    std::fs::write(&tmp, state.render())?;
    std::fs::rename(&tmp, &path)
}

/// One entry as a reader finds it.
#[derive(Debug)]
pub(crate) struct Entry {
    pub(crate) token: RunToken,
    /// Its flock is held: a supervisor (or, before the spawn, its launcher)
    /// is executing the protocol.
    pub(crate) held: bool,
    /// `None` when the record could not be read or parsed. Such an entry
    /// stays outstanding: nothing can be proved about it.
    pub(crate) intent: Option<Intent>,
    pub(crate) state: Option<State>,
}

/// Every entry in the registry. A missing directory is an empty registry;
/// any other failure to read it is an error, never an empty answer.
pub(crate) fn entries(dir: &Path) -> std::io::Result<Vec<Entry>> {
    let listing = match std::fs::read_dir(dir) {
        Ok(listing) => listing,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for item in listing {
        let item = item?;
        let name = item.file_name();
        let Some((stem, ext)) = name.to_str().and_then(|n| n.split_once('.')) else {
            continue;
        };
        if ext != LOCK_EXT {
            continue;
        }
        let Some(token) = RunToken::parse(stem) else {
            continue;
        };
        if let Some(entry) = read_entry(dir, &token)? {
            out.push(entry);
        }
    }
    out.sort_by_key(|e| e.intent.as_ref().map_or(0, |i| i.launched));
    Ok(out)
}

/// Read one entry; `None` when it was retired between the listing and here.
pub(crate) fn read_entry(dir: &Path, token: &RunToken) -> std::io::Result<Option<Entry>> {
    let mut file = match std::fs::File::open(lock_path(dir, token)) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    // The probe's own flock goes with `file` at the end of this function;
    // a free entry stays free, since nothing ever locks an existing entry.
    let held = !try_lock(&file)?;
    // A record that cannot be read is an error, never an absent record: only
    // one that reads but does not parse is the incomplete publication of a
    // launcher that died mid-write, before any supervisor existed.
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let intent = Intent::parse(&text);
    let state = match std::fs::read_to_string(state_path(dir, token)) {
        Ok(text) => State::parse(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    Ok(Some(Entry {
        token: token.clone(),
        held,
        intent,
        state,
    }))
}

/// Retire an entry whose flock is free: remove its state and its record, and
/// the served executables' links from its generation directory, keeping the
/// log for a later look. Refuses (`Ok(false)`) an entry whose flock is held.
pub(super) fn retire(
    dir: &Path,
    token: &RunToken,
    intent: Option<&Intent>,
) -> std::io::Result<bool> {
    let _registry = RegistryLock::acquire(dir)?;
    let path = lock_path(dir, token);
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(e) => return Err(e),
    };
    if !try_lock(&file)? {
        return Ok(false);
    }
    if let Some(intent) = intent {
        for slot in [super::PROGRAM_SLOT, super::WITH_SLOT] {
            match std::fs::remove_dir_all(intent.generation.join(slot)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
    }
    match std::fs::remove_file(state_path(dir, token)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    std::fs::remove_file(&path)?;
    Ok(true)
}

/// The scope an intent records: this host's boot id and this process's PID
/// and time namespaces, or `None` when any of them cannot be read. A start
/// refuses without it, since an intent with a gap could never be judged.
pub(super) fn local_scope() -> Option<(String, String, String)> {
    let ns = |path: &str| crate::process::namespace_inode(path).map(|i| i.get().to_string());
    Some((
        crate::process::local_boot_id()?.as_str().to_owned(),
        ns("/proc/self/ns/pid")?,
        ns("/proc/thread-self/ns/time")?,
    ))
}

/// How a recorded scope relates to this reader's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Scope {
    /// Same boot and same PID and time namespaces: a scan here sees the
    /// generation's processes.
    Here,
    /// Positively an earlier boot: none of its processes can be running.
    EarlierBoot,
    /// Another namespace, or a scope that cannot be established; nothing can
    /// be judged from here.
    Unknown,
}

/// Judge an intent's recorded scope from this reader. Every gap - a field
/// recorded empty, a boot id or namespace this reader cannot read - is
/// [`Scope::Unknown`], never a match.
pub(super) fn scope_of(intent: &Intent) -> Scope {
    let (Some(recorded), Some(local)) = (
        BootId::new(&intent.boot_id),
        crate::process::local_boot_id(),
    ) else {
        return Scope::Unknown;
    };
    if recorded != local {
        return Scope::EarlierBoot;
    }
    if intent.pid_ns.is_empty() || intent.time_ns.is_empty() {
        return Scope::Unknown;
    }
    if crate::process::shares_namespace_tokens(&intent.pid_ns, &intent.time_ns) {
        Scope::Here
    } else {
        Scope::Unknown
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn intent(name: &str) -> Intent {
        Intent {
            token: RunToken::mint().unwrap(),
            project: "demo".into(),
            root: "/r".into(),
            name: name.into(),
            target: "srv".into(),
            generation: "/r/state/serve/x".into(),
            program: "/r/state/serve/x/program/srv".into(),
            with: None,
            args: vec!["--port".into(), "a\nb".into(), String::new()],
            cwd: "/r".into(),
            log: "/r/state/serve/x/log".into(),
            grace: Duration::from_millis(5000),
            boot_id: "b".into(),
            pid_ns: "1".into(),
            time_ns: "2".into(),
            launched: 7,
        }
    }

    #[test]
    fn records_round_trip_and_incomplete_ones_are_refused() {
        let i = intent("srv");
        assert_eq!(Intent::parse(&i.render()), Some(i.clone()));
        let with = Intent {
            with: Some("/w".into()),
            ..i
        };
        assert_eq!(Intent::parse(&with.render()), Some(with));
        assert_eq!(Intent::parse("token=x\n"), None);

        let s = State {
            supervisor: ProcIdentity::new(Pid::new(5).unwrap(), StartTicks::new(9)),
            server: None,
            phase: Phase::Supervising,
        };
        assert_eq!(State::parse(&s.render()), Some(s.clone()));
        let s = State {
            server: Some(ProcIdentity::new(Pid::new(6).unwrap(), StartTicks::new(10))),
            phase: Phase::Serving,
            ..s
        };
        assert_eq!(State::parse(&s.render()), Some(s));
    }

    /// A published entry reads as held while its flock lives, as free once
    /// it is dropped, and retiring refuses a held one.
    #[test]
    fn an_entry_is_held_exactly_while_its_flock_lives() {
        let dir = crate::test_scratch::scratch("serve", "entries");
        let i = intent("srv");
        let held = publish(&dir, &i).unwrap();
        let found = entries(&dir).unwrap();
        assert_eq!(found.len(), 1);
        assert!(found[0].held);
        assert_eq!(found[0].intent.as_ref(), Some(&i));
        assert!(found[0].state.is_none());
        assert!(!retire(&dir, &i.token, Some(&i)).unwrap());
        drop(held);
        let found = entries(&dir).unwrap();
        assert!(!found[0].held);
        assert!(retire(&dir, &i.token, Some(&i)).unwrap());
        assert!(entries(&dir).unwrap().is_empty());
    }
}
