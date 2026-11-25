//! Trimming a filesystem under the brokkr lock: `brokkr fstrim` and the bench
//! trim reset both come through here.
//!
//! The privileged step is `brokkr-fstrim` (`src/bin/fstrim_helper.rs`),
//! installed by the operator at [`HELPER_PATH`]. The install chain is walked
//! once per use through held `O_PATH` descriptors without following links,
//! each directory and the file root-owned and writable by nobody else, into
//! a pinned [`Helper`]; the probe and every trim of that use run that one
//! inode, executed through `/proc/self/fd`, so a replacement of the path
//! after the walk is never run.
//!
//! The helper gets the target as an open directory descriptor and a
//! duplicate of the held flock's descriptor, both above the spawn's own
//! descriptors. Exclusion rests on that duplicate, not on the reap: the
//! supervisor reaps the helper on every ordinary ending, and on a wait
//! failure, an unwind or a killed brokkr the duplicate keeps the lock held
//! until the helper exits. Its stdout is one line per stage; [`parse`]
//! accepts only the complete lines of an in-order transcript, so a stopped
//! run keeps exactly the stages it finished.

use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::path::{Path, PathBuf};

use crate::error::DevError;

/// Where the operator installs the helper (`brokkr man fstrim`).
pub(crate) const HELPER_PATH: &str = "/usr/local/libexec/brokkr-fstrim";

/// The helper's line protocol this brokkr speaks.
pub(crate) const PROTOCOL: u32 = 1;

const EXT4_SUPER_MAGIC: i64 = 0xEF53;

/// The install fix-it text every refusal names.
pub(crate) const INSTALL_HINT: &str = "install the copy cargo install put beside brokkr: \
    sudo install -D -o root -g \"$(id -gn)\" -m 0750 ~/.cargo/bin/brokkr-fstrim \
    /usr/local/libexec/brokkr-fstrim, then sudo setcap cap_sys_admin+ep \
    /usr/local/libexec/brokkr-fstrim (brokkr man fstrim)";

fn last_error() -> std::io::Error {
    std::io::Error::last_os_error()
}

fn stat_fd(fd: &OwnedFd) -> std::io::Result<libc::stat> {
    // SAFETY: a zeroed out-struct and a valid descriptor.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &raw mut st) } != 0 {
        return Err(last_error());
    }
    Ok(st)
}

fn openat(dir: Option<&OwnedFd>, name: &str, flags: i32) -> std::io::Result<OwnedFd> {
    let c = std::ffi::CString::new(name).map_err(|_| std::io::Error::other("NUL in path"))?;
    let base = dir.map_or(libc::AT_FDCWD, std::os::fd::AsRawFd::as_raw_fd);
    // SAFETY: a valid C string and a valid or AT_FDCWD base.
    let fd = unsafe { libc::openat(base, c.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(last_error());
    }
    // SAFETY: openat just returned this descriptor, owned by nobody else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Whether a node is root-owned and writable by root alone.
fn trusted(st: &libc::stat) -> bool {
    st.st_uid == 0 && st.st_mode & 0o022 == 0
}

/// The validated helper: an `O_PATH` descriptor on the installed inode.
pub(crate) struct Helper {
    fd: OwnedFd,
}

/// Walk [`HELPER_PATH`] from `/` through held descriptors, refusing a link,
/// a node not owned by root, or one anyone else may write.
pub(crate) fn open_helper() -> Result<Helper, String> {
    let mut dir = openat(None, "/", libc::O_PATH | libc::O_DIRECTORY)
        .map_err(|e| format!("cannot open /: {e}"))?;
    let mut walked = String::new();
    let parts: Vec<&str> = HELPER_PATH.split('/').filter(|p| !p.is_empty()).collect();
    let check = |fd: &OwnedFd, at: &str| -> Result<libc::stat, String> {
        let st = stat_fd(fd).map_err(|e| format!("cannot stat {at}: {e}"))?;
        if !trusted(&st) {
            return Err(format!(
                "{at} is not root-owned and writable by root alone, so the helper below it \
                 cannot be trusted"
            ));
        }
        Ok(st)
    };
    check(&dir, "/")?;
    for (i, part) in parts.iter().enumerate() {
        walked.push('/');
        walked.push_str(part);
        let last = i + 1 == parts.len();
        let flags = if last {
            libc::O_PATH | libc::O_NOFOLLOW
        } else {
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_DIRECTORY
        };
        let next = openat(Some(&dir), part, flags).map_err(|e| {
            if e.raw_os_error() == Some(libc::ENOENT) {
                format!("{walked} does not exist")
            } else {
                format!("cannot open {walked}: {e}")
            }
        })?;
        let st = check(&next, &walked)?;
        if last && st.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(format!("{walked} is not a regular file"));
        }
        dir = next;
    }
    Ok(Helper { fd: dir })
}

/// Move a descriptor to 10 or above, close-on-exec, so no descriptor a spawn
/// makes for itself collides with it.
fn high_dup(fd: &impl std::os::fd::AsRawFd) -> std::io::Result<OwnedFd> {
    // SAFETY: a valid descriptor.
    let new = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) };
    if new < 0 {
        return Err(last_error());
    }
    // SAFETY: fcntl just returned this descriptor, owned by nobody else.
    Ok(unsafe { OwnedFd::from_raw_fd(new) })
}

/// What `--probe` reported, or why it could not run.
pub(crate) struct Probe {
    pub protocol: Option<u32>,
    pub capable: bool,
    pub detail: String,
}

/// Ask the pinned helper whether it runs with CAP_SYS_ADMIN. Only a run that
/// ended as the probe ends, exit 0 with the capability or exit 1 without it,
/// and printed both lines whole, is an answer; anything else is `Err`.
pub(crate) fn probe(helper: &Helper) -> Result<Probe, String> {
    let out = run_helper(helper, &["--probe"], &[])
        .map_err(|e| format!("{HELPER_PATH} could not run: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (lines, tail) = complete_lines(&text);
    if tail || lines.len() != 2 {
        return Err(format!(
            "{HELPER_PATH} --probe printed {} line(s){}, expected 2",
            lines.len(),
            if tail { " and a partial one" } else { "" }
        ));
    }
    let protocol = lines
        .first()
        .and_then(|l| l.strip_prefix("brokkr-fstrim "))
        .and_then(|v| v.trim().parse().ok());
    let detail = lines.get(1).copied().unwrap_or("no capability line").to_owned();
    let capable = match (&out.ending, detail.as_str()) {
        (crate::output::child::Ending::Exited(0), "cap_sys_admin effective") => true,
        (crate::output::child::Ending::Exited(1), d) if d.starts_with("cap_sys_admin ") => false,
        (ending, _) => {
            return Err(format!(
                "{HELPER_PATH} --probe ended as {}",
                ending_words(ending)
            ));
        }
    };
    if out.output_cut || out.stdout_truncated {
        return Err(format!("{HELPER_PATH} --probe output was cut"));
    }
    Ok(Probe {
        protocol,
        capable,
        detail,
    })
}

/// Pin the installed helper and probe it: its install validated, its
/// protocol this brokkr's, and CAP_SYS_ADMIN effective. The returned
/// [`Helper`] is the inode every trim of this use runs. `Err` is the
/// refusal text.
pub(crate) fn require_ready() -> Result<Helper, String> {
    let helper = open_helper().map_err(|e| format!("{e}; {INSTALL_HINT}"))?;
    let probe = probe(&helper).map_err(|e| format!("{e}; {INSTALL_HINT}"))?;
    if probe.protocol != Some(PROTOCOL) {
        return Err(format!(
            "{HELPER_PATH} speaks protocol {}, this brokkr {PROTOCOL}; reinstall it. {INSTALL_HINT}",
            probe
                .protocol
                .map_or_else(|| "unknown".to_owned(), |p| p.to_string())
        ));
    }
    if !probe.capable {
        return Err(format!(
            "{HELPER_PATH} runs without CAP_SYS_ADMIN ({}); {INSTALL_HINT}",
            probe.detail
        ));
    }
    Ok(helper)
}

/// The `brokkr env` row: whether a trim could run, and if not, why.
pub(crate) fn env_status() -> String {
    match open_helper().and_then(|h| probe(&h)) {
        Err(why) => format!("unavailable: {why}"),
        Ok(p) if p.protocol != Some(PROTOCOL) => format!(
            "{HELPER_PATH} speaks protocol {}, this brokkr {PROTOCOL}",
            p.protocol.map_or_else(|| "unknown".to_owned(), |v| v.to_string())
        ),
        Ok(p) if !p.capable => format!("{HELPER_PATH} without CAP_SYS_ADMIN ({})", p.detail),
        Ok(_) => format!("{HELPER_PATH} (protocol {PROTOCOL}, cap_sys_admin, ext4 only)"),
    }
}

/// Spawn the helper through its validated descriptor with `inherit` made
/// inheritable in the child, and supervise it to its reap.
fn run_helper(
    helper: &Helper,
    args: &[&str],
    inherit: &[i32],
) -> std::io::Result<crate::output::child::SupervisedChild> {
    use std::os::unix::process::CommandExt as _;
    let exe = high_dup(&helper.fd)?;
    let mut command = crate::process::spawn::ChildPolicy::plain_command(format!(
        "/proc/self/fd/{}",
        exe.as_raw_fd()
    ));
    command.arg0("brokkr-fstrim").args(args);
    command.stdin(std::process::Stdio::null());
    crate::process::spawn::capture_stdio(&mut command);
    let keep = inherit.to_vec();
    // SAFETY: only fcntl on descriptors prepared before the fork.
    unsafe {
        command.pre_exec(move || {
            for &fd in &keep {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    crate::process::spawn::ChildPolicy::plain().apply_runtime_policy(&mut command);
    let _interrupts = crate::shutdown::SigtermGuard::install();
    let start = std::time::Instant::now();
    let child = crate::process::spawn::spawn(command, None).map_err(|error| match error {
        DevError::Spawn { error, .. } => error,
        DevError::Interrupted => {
            std::io::Error::new(std::io::ErrorKind::Interrupted, "interrupted")
        }
        other => std::io::Error::other(other.to_string()),
    })?;
    let hold = crate::lockfile::live_hold();
    if let Some(hold) = &hold {
        hold.set_child_pid(child.id());
    }
    let run = crate::output::child::supervise_child(
        child,
        "brokkr-fstrim",
        start,
        crate::output::child::ChildLimit::None,
        crate::shutdown::ChildIsolation::Inherit,
        crate::output::child::ChildOptions::DEFAULT,
        |_| None,
    );
    if let Some(hold) = &hold {
        hold.clear_child_pid();
    }
    drop(exe);
    Ok(run)
}

/// One stage the helper reported: finished, failed with an errno, or never
/// reported (not reached, or stopped before it said).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum Stage {
    Ok {
        ns: u64,
    },
    Failed {
        errno: i32,
    },
    Unreported,
}

/// What one trim of one filesystem did, as the helper reported it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Trim {
    /// The helper's protocol, from its first line.
    pub protocol: Option<u32>,
    /// `MAJOR:MINOR` of the filesystem, as the helper read it from the
    /// descriptor.
    pub target: Option<String>,
    /// The filesystem's `f_fsid` in hex, evidence beside the device (ext4
    /// derives it from the volume UUID, so clones share it).
    pub fsid: Option<String>,
    /// Whether the helper saw the flock on its descriptor; `false` when
    /// procfs could not tell, which is only a diagnostic.
    pub lock_seen: bool,
    pub sync: Stage,
    pub trim: Stage,
    /// The length FITRIM returned: what the filesystem reports it passed to
    /// discard, not what the device discarded.
    pub reported_bytes: Option<u64>,
    /// A refusal before the stages: wrong filesystem, not a directory.
    pub refused: Option<String>,
    /// The first line that broke the transcript's order or form; nothing
    /// after it was read.
    pub malformed: Option<String>,
    /// Whether the capture was cut or truncated, so lines may be missing.
    pub output_cut: bool,
    /// The supervisor could not confirm the helper exited (a wait failure):
    /// it may still be running on the inherited lock, so no other trim may
    /// start in this process.
    pub reap_unconfirmed: bool,
    /// How the helper ended, in words: `exit 0`, `exit 1`, `signal 9`,
    /// `interrupted`.
    pub ending: String,
}

impl Trim {
    /// A finished, well-formed run of this brokkr's protocol: every stage in
    /// order and successful, nothing cut, exit 0.
    pub(crate) fn completed(&self) -> bool {
        self.ending == "exit 0"
            && self.protocol == Some(PROTOCOL)
            && self.target.is_some()
            && self.refused.is_none()
            && self.malformed.is_none()
            && !self.output_cut
            && matches!(self.sync, Stage::Ok { .. })
            && matches!(self.trim, Stage::Ok { .. })
            && self.reported_bytes.is_some()
    }

    /// One line for the console.
    pub(crate) fn describe(&self) -> String {
        if let Some(why) = &self.refused {
            return format!("refused: {why}");
        }
        if let Some(line) = &self.malformed {
            return format!("unreadable helper output at {line:?} ({})", self.ending);
        }
        let stage = |name: &str, s: &Stage| match s {
            Stage::Ok { ns } => format!(
                "{name} {}",
                crate::output::format_duration(std::time::Duration::from_nanos(*ns))
            ),
            Stage::Failed { errno } => format!(
                "{name} failed: {}",
                std::io::Error::from_raw_os_error(*errno)
            ),
            Stage::Unreported => format!("{name} unreported"),
        };
        let mut text = format!("{}, {}", stage("sync", &self.sync), stage("trim", &self.trim));
        if let Some(bytes) = self.reported_bytes {
            text.push_str(&format!(
                ", FITRIM reported {}",
                crate::output::format_bytes(bytes)
            ));
        }
        if !self.completed() {
            text.push_str(&format!(" ({})", self.ending));
        }
        text
    }
}

fn ending_words(ending: &crate::output::child::Ending) -> String {
    use crate::output::child::Ending;
    match ending {
        Ending::Exited(code) => format!("exit {code}"),
        Ending::Signalled(sig) => format!("signal {sig}"),
        Ending::Interrupted => "interrupted".to_owned(),
        Ending::StopMarker(m) => format!("stopped at {m}"),
        Ending::Deadline(d) => format!("deadline {}", crate::output::format_duration(*d)),
        Ending::WaitFailed(why) => format!("wait failed: {why}"),
    }
}

/// The lines a run printed whole, and whether a partial line followed them:
/// a last line without its newline was cut mid-write and is no record.
fn complete_lines(text: &str) -> (Vec<&str>, bool) {
    let mut lines: Vec<&str> = text.split('\n').collect();
    // The piece after the last newline: empty when the text ended with one,
    // else a partial line.
    let tail = lines.pop().is_some_and(|t| !t.is_empty());
    (lines, tail)
}

/// `MAJOR:MINOR` as the helper prints a device, as a `dev_t`.
fn parse_dev(text: &str) -> Option<u64> {
    let (major, minor) = text.split_once(':')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(major) || !digits(minor) {
        return None;
    }
    Some(libc::makedev(major.parse().ok()?, minor.parse().ok()?))
}

/// An `f_fsid` as the helper prints it: sixteen lowercase hex digits.
fn is_fsid(text: &str) -> bool {
    text.len() == 16 && text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Where the transcript stands: which line the helper prints next.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Next {
    Version,
    Lock,
    Target,
    Sync,
    Trim,
    /// A terminal line was read (a refusal, a failure, the trim): nothing
    /// may follow.
    End,
}

/// Parse the helper's transcript strictly: complete lines only, in the
/// order it prints them, each in its exact form, the target the device
/// `expected` (when given) names. The first line out of order or form is
/// kept as `malformed` and ends the reading, so a run is credited only with
/// the stages a well-formed prefix reports; a partial line after them
/// counts as a cut capture, which no completed run has.
pub(crate) fn parse(stdout: &str, ending: String, output_cut: bool, expected: Option<u64>) -> Trim {
    let (lines, tail) = complete_lines(stdout);
    let mut trim = Trim {
        protocol: None,
        target: None,
        fsid: None,
        lock_seen: false,
        sync: Stage::Unreported,
        trim: Stage::Unreported,
        reported_bytes: None,
        refused: None,
        malformed: None,
        output_cut: output_cut || tail,
        reap_unconfirmed: ending.starts_with("wait failed"),
        ending,
    };
    let mut next = Next::Version;
    for line in lines {
        let words: Vec<&str> = line.split(' ').collect();
        let parsed = match (next, words.as_slice()) {
            (Next::End, _) => None,
            // The helper refuses only before its lock line: usage, protocol,
            // capability, target.
            (Next::Version | Next::Lock, ["refused", rest @ ..]) if !rest.is_empty() => {
                trim.refused = Some(rest.join(" "));
                Some(Next::End)
            }
            (Next::Version, ["brokkr-fstrim", v]) => v.parse().ok().map(|v| {
                trim.protocol = Some(v);
                Next::Lock
            }),
            (Next::Lock, ["lock", "held"]) => {
                trim.lock_seen = true;
                Some(Next::Target)
            }
            (Next::Lock, ["lock", "unverified"]) => Some(Next::Target),
            (Next::Target, ["target", dev, fsid])
                if is_fsid(fsid)
                    && parse_dev(dev).is_some_and(|d| expected.is_none_or(|e| e == d)) =>
            {
                trim.target = Some((*dev).to_owned());
                trim.fsid = Some((*fsid).to_owned());
                Some(Next::Sync)
            }
            (Next::Sync, ["sync", "ok", ns]) => ns.parse().ok().map(|ns| {
                trim.sync = Stage::Ok { ns };
                Next::Trim
            }),
            (Next::Sync, ["sync", "failed", e]) => e.parse().ok().map(|errno| {
                trim.sync = Stage::Failed { errno };
                Next::End
            }),
            (Next::Trim, ["trim", "ok", bytes, ns]) => match (bytes.parse(), ns.parse()) {
                (Ok(bytes), Ok(ns)) => {
                    trim.reported_bytes = Some(bytes);
                    trim.trim = Stage::Ok { ns };
                    Some(Next::End)
                }
                _ => None,
            },
            (Next::Trim, ["trim", "failed", e]) => e.parse().ok().map(|errno| {
                trim.trim = Stage::Failed { errno };
                Next::End
            }),
            _ => None,
        };
        match parsed {
            Some(state) => next = state,
            None => {
                trim.malformed = Some(line.to_owned());
                break;
            }
        }
    }
    trim
}

/// A target opened and checked: an ext4 directory, by its device.
pub(crate) struct Target {
    pub path: PathBuf,
    pub dir: OwnedFd,
    pub dev: u64,
}

/// Open `path` as a directory and check it is on ext4.
pub(crate) fn open_target(path: &Path) -> Result<Target, String> {
    let text = path.to_str().ok_or_else(|| format!("{} is not UTF-8", path.display()))?;
    let dir = openat(None, text, libc::O_RDONLY | libc::O_DIRECTORY)
        .map_err(|e| format!("{}: {e} (a trim target is a directory)", path.display()))?;
    let st = stat_fd(&dir).map_err(|e| format!("{}: {e}", path.display()))?;
    // SAFETY: a zeroed out-struct and a valid descriptor.
    let mut sfs: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(dir.as_raw_fd(), &raw mut sfs) } != 0 {
        return Err(format!("{}: {}", path.display(), last_error()));
    }
    if sfs.f_type != EXT4_SUPER_MAGIC {
        return Err(format!(
            "{} is not on ext4 (filesystem magic 0x{:x}); brokkr trims ext4 only",
            path.display(),
            sfs.f_type
        ));
    }
    Ok(Target {
        path: path.to_path_buf(),
        dir,
        dev: st.st_dev,
    })
}

/// Trim the filesystem holding `dir` with the pinned `helper` under `hold`.
/// `Err` only when the helper could not be started; everything it did is
/// in the returned [`Trim`].
pub(crate) fn trim(
    helper: &Helper,
    dir: &OwnedFd,
    hold: &crate::lockfile::LockGuard,
) -> Result<Trim, DevError> {
    let expected = stat_fd(dir)?.st_dev;
    let dir = high_dup(dir)?;
    let lock = hold.duplicate_flock()?;
    let protocol = PROTOCOL.to_string();
    let ppid = std::process::id().to_string();
    let dir_fd = dir.as_raw_fd().to_string();
    let lock_fd = lock.as_raw_fd().to_string();
    let run = run_helper(
        helper,
        &["--trim", &protocol, &ppid, &dir_fd, &lock_fd],
        &[dir.as_raw_fd(), lock.as_raw_fd()],
    );
    let run = match run {
        Ok(run) => run,
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return Err(DevError::Interrupted),
        Err(e) => return Err(DevError::Refused(format!("{HELPER_PATH} could not start: {e}"))),
    };
    Ok(parse(
        &String::from_utf8_lossy(&run.stdout),
        ending_words(&run.ending),
        run.output_cut || run.stdout_truncated,
        Some(expected),
    ))
}

/// `brokkr fstrim [PATH...]`: every target validated before the first trim,
/// one trim per distinct filesystem, each reported.
pub(crate) fn cmd_fstrim(paths: &[PathBuf], default: Option<PathBuf>) -> Result<(), DevError> {
    let paths: Vec<PathBuf> = if paths.is_empty() {
        vec![match default {
            Some(p) => p,
            None => std::env::current_dir()?,
        }]
    } else {
        paths.to_vec()
    };
    let mut targets: Vec<Target> = Vec::new();
    for path in &paths {
        let target = open_target(path).map_err(DevError::Refused)?;
        if targets.iter().all(|t| t.dev != target.dev) {
            targets.push(target);
        }
    }
    let helper = require_ready().map_err(DevError::Refused)?;
    let hold = crate::lockfile::require_hold(|| {
        DevError::Internal("brokkr fstrim runs under the lock".to_owned())
    })?;
    let mut failed = 0_usize;
    for target in &targets {
        let result = trim(&helper, &target.dir, &hold)?;
        let line = format!("{}: {}", crate::output::path(&target.path), result.describe());
        if result.completed() {
            crate::output::run_msg(&line);
        } else {
            failed += 1;
            crate::output::error(&line);
        }
        if result.ending == "interrupted" {
            return Err(DevError::Interrupted);
        }
        if result.reap_unconfirmed {
            return Err(DevError::Reported(
                "brokkr could not confirm the helper exited, so no further trim starts; \
                 the lock stays held until it does"
                    .to_owned(),
            ));
        }
    }
    if failed > 0 {
        return Err(DevError::Reported(format!("{failed} trim(s) did not complete")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const HEAD: &str = "brokkr-fstrim 1\nlock held\ntarget 259:2 0123456789abcdef\n";

    fn run(body: &str, ending: &str) -> Trim {
        parse(&format!("{HEAD}{body}"), ending.to_owned(), false, None)
    }

    const OK: &str = "sync ok 1\ntrim ok 1 1\n";

    /// The target must be a device in form, match the one opened, and carry
    /// a sixteen-digit fsid.
    #[test]
    fn the_target_line_is_validated() {
        let head = |target: &str| format!("brokkr-fstrim 1\nlock held\ntarget {target}\n{OK}");
        let good = parse(&head("259:2 0123456789abcdef"), "exit 0".into(), false, None);
        assert!(good.completed());
        for target in ["bogus nope", "259:2 xyz", "259: 0123456789abcdef", "a:b 0123456789abcdef"] {
            let t = parse(&head(target), "exit 0".into(), false, None);
            assert!(t.malformed.is_some() && !t.completed(), "{target}");
        }
        let elsewhere = parse(
            &head("259:2 0123456789abcdef"),
            "exit 0".into(),
            false,
            Some(libc::makedev(259, 3)),
        );
        assert!(!elsewhere.completed());
        let same = parse(
            &head("259:2 0123456789abcdef"),
            "exit 0".into(),
            false,
            Some(libc::makedev(259, 2)),
        );
        assert!(same.completed());
    }

    /// A partial line after a complete success is a cut capture.
    #[test]
    fn a_partial_tail_after_success_fails_completion() {
        let t = run(&format!("{OK}extra"), "exit 0");
        assert!(t.output_cut);
        assert!(!t.completed());
    }

    #[test]
    fn a_wait_failure_marks_the_reap_unconfirmed() {
        let t = run(OK, "wait failed: ECHILD");
        assert!(t.reap_unconfirmed);
        assert!(!t.completed());
        assert!(!run(OK, "exit 0").reap_unconfirmed);
    }

    #[test]
    fn a_full_run_parses_as_completed() {
        let t = run("sync ok 1500\ntrim ok 4096 2000000\n", "exit 0");
        assert_eq!(t.protocol, Some(1));
        assert!(t.lock_seen);
        assert_eq!(t.target.as_deref(), Some("259:2"));
        assert_eq!(t.fsid.as_deref(), Some("0123456789abcdef"));
        assert_eq!(t.sync, Stage::Ok { ns: 1500 });
        assert_eq!(t.trim, Stage::Ok { ns: 2_000_000 });
        assert_eq!(t.reported_bytes, Some(4096));
        assert!(t.completed(), "{t:?}");
    }

    /// A stop after the sync keeps the sync and leaves the trim unreported.
    #[test]
    fn a_stopped_run_keeps_the_stages_it_finished() {
        let t = run("sync ok 10\n", "interrupted");
        assert_eq!(t.sync, Stage::Ok { ns: 10 });
        assert_eq!(t.trim, Stage::Unreported);
        assert!(!t.completed());
        assert!(t.describe().contains("trim unreported"), "{}", t.describe());
    }

    /// A last line without its newline was cut mid-write: not credited.
    #[test]
    fn an_unterminated_last_line_is_no_record() {
        let t = run("sync ok 10\ntrim ok 4096 20", "exit 0");
        assert_eq!(t.trim, Stage::Unreported);
        assert!(!t.completed());
    }

    #[test]
    fn failures_and_refusals_parse() {
        let t = run("sync ok 1\ntrim failed 1\n", "exit 1");
        assert_eq!(t.trim, Stage::Failed { errno: 1 });
        assert!(!t.completed());
        let t = parse(
            "brokkr-fstrim 1\nrefused fstype 0x58465342\n",
            "exit 2".to_owned(),
            false,
            None,
        );
        assert_eq!(t.refused.as_deref(), Some("fstype 0x58465342"));
        assert!(t.describe().starts_with("refused"));
    }

    /// Out of order, a success after a failure, a malformed errno, a line
    /// after the end, a skipped stage: each stops the reading there.
    #[test]
    fn a_transcript_out_of_order_or_form_is_never_completed() {
        let bad = [
            "trim ok 1 1\nsync ok 1\n",
            "sync failed 5\nsync ok 1\ntrim ok 1 1\n",
            "sync failed x\n",
            "sync ok 1\ntrim ok 1 1\ntrim ok 2 2\n",
            "sync ok 1\nrefused late\n",
        ];
        for body in bad {
            let t = run(body, "exit 0");
            assert!(!t.completed(), "{body:?}: {t:?}");
            assert!(t.malformed.is_some(), "{body:?}: {t:?}");
        }
        let skipped = parse(
            "brokkr-fstrim 1\nsync ok 1\ntrim ok 1 1\n",
            "exit 0".to_owned(),
            false,
            None,
        );
        assert!(!skipped.completed());
    }

    /// Exit zero alone proves nothing: another protocol, a cut capture, or
    /// no trim line all fail completion.
    #[test]
    fn exit_zero_needs_the_whole_protocol() {
        assert!(!run("sync ok 1\n", "exit 0").completed());
        let other = parse(
            "brokkr-fstrim 2\nlock held\ntarget 1:1 00\nsync ok 1\ntrim ok 1 1\n",
            "exit 0".to_owned(),
            false,
            None,
        );
        assert!(!other.completed());
        let cut = parse(
            &format!("{HEAD}sync ok 1\ntrim ok 1 1\n"),
            "exit 0".to_owned(),
            true,
            None,
        );
        assert!(!cut.completed());
    }
}
