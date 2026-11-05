//! `brokkr-results-server deploy`: restart this host's server, if it has
//! one, and confirm that the endpoint it declares answers with this build.
//!
//! Meant to run after every install on every machine, as a fleet tool's hook
//! does, so the host decides by its own state: a declaration file, fixed
//! under the home directory and read without XDG lookup, so a hook with a
//! stripped environment finds the same file a login shell does. No file is
//! no server here, a success that touches nothing. Nothing about a service
//! manager is compiled in: the declaration names the restart command, and the
//! proof comes from the new process itself, which reports the digest of the
//! executable it runs on its identity route. Success means the restart
//! command succeeded and, within the deadline, the declared endpoint answered
//! a fresh probe with this executable's digest; not that the store answers
//! bench requests, that the process stays up, or which build serves once
//! overlapping deploys end.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::identity::{self, SERVER};
use crate::store::wire;

/// Where the declaration lives, relative to the home directory.
pub(crate) const DECLARATION: &str = ".config/brokkr-results-server/deploy.toml";

/// The deadline when the declaration names none.
const DEFAULT_TIMEOUT_SECS: u64 = 300;

/// One probe's own bound, within what remains of the deadline.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// The largest identity answer read.
const ANSWER_LIMIT: u64 = 64 * 1024;

/// The first and the longest wait between probes.
const BACKOFF_FIRST: Duration = Duration::from_millis(100);
const BACKOFF_MAX: Duration = Duration::from_secs(2);

/// How often a running restart command is checked on.
const RESTART_POLL: Duration = Duration::from_millis(50);

/// This host's results server, as its operator declared it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Declaration {
    /// The endpoint probed: the server on this host directly, normally its
    /// loopback listen address, `http://127.0.0.1:8650/bench`.
    pub(crate) url: String,
    /// The command that restarts it, run directly (no shell) from the home
    /// directory with the environment deploy was given.
    pub(crate) restart: Vec<String>,
    /// Seconds for the restart command and every probe together, counted on
    /// the monotonic clock, so time asleep does not count.
    #[serde(default = "default_timeout")]
    pub(crate) timeout_secs: u64,
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

/// What the watchdog allows past the deadline, so the deploy's own account
/// of a deadline it reached arrives before the watchdog's.
const GRACE: Duration = Duration::from_secs(1);

/// The one line deploy says when it is done. Best effort: the deploy has
/// happened or not by now, and a closed stdout changes neither.
fn say(message: &str) {
    use std::io::Write;
    match writeln!(std::io::stdout(), "brokkr-results-server: {message}") {
        Ok(()) | Err(_) => {}
    }
}

/// The command: find the declaration under `$HOME` and act on it. The
/// work runs on a thread of its own and the caller waits for it until the
/// deadline (and [`GRACE`]), so nothing it blocks on, a spawn, a reap or a
/// TLS handshake, can hold the deploy past its declared bound.
pub(crate) fn run() -> Result<(), String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .ok_or("HOME is unset or not an absolute path")?;
    let path = home.join(DECLARATION);
    let Some(declaration) = load(&path)? else {
        say(&format!(
            "no {} here: this host serves no results server; nothing to deploy",
            path.display()
        ));
        return Ok(());
    };
    let deadline = declaration.deadline(Instant::now())?;
    let url = declaration.url.clone();
    let timeout = declaration.timeout_secs;
    let digest = watched(deadline, timeout, move || {
        let digest = identity::executable_digest()?;
        converge(&declaration, &digest, &home, deadline)?;
        Ok(digest)
    })?;
    say(&format!("{url} answers with this build ({digest})"));
    Ok(())
}

/// Run `work` on a thread of its own and wait for it until `deadline` and
/// [`GRACE`]. Past that the caller has its answer, and `work` is abandoned
/// to end with the process.
pub(crate) fn watched<T: Send + 'static>(
    deadline: Instant,
    timeout_secs: u64,
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let (done, outcome) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("deploy".into())
        .spawn(move || {
            // The caller may have stopped waiting; then nobody needs this.
            drop(done.send(work()));
        })
        .map_err(|e| format!("cannot start the deploy: {e}"))?;
    let wait = deadline.saturating_duration_since(Instant::now()) + GRACE;
    outcome.recv_timeout(wait).unwrap_or_else(|_| {
        Err(format!(
            "the deploy did not finish within {timeout_secs} s; what it started may still complete"
        ))
    })
}

/// The declaration at `path`, none when there is no file. A link to
/// nothing is a broken declaration, not an absent one.
pub(crate) fn load(path: &Path) -> Result<Option<Declaration>, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot inspect {}: {e}", path.display())),
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let declaration: Declaration =
        toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    declaration
        .check()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Some(declaration))
}

impl Declaration {
    fn check(&self) -> Result<(), String> {
        if self.url.contains(['?', '#']) {
            return Err(format!(
                "url {} has a query or fragment; deploy adds its own probe",
                self.url
            ));
        }
        let uri: ureq::http::Uri = self
            .url
            .parse()
            .map_err(|e| format!("url {} does not parse: {e}", self.url))?;
        if !matches!(uri.scheme_str(), Some("http" | "https")) {
            return Err(format!("url {} is not http or https", self.url));
        }
        if uri.host().is_none_or(str::is_empty) {
            return Err(format!("url {} names no host", self.url));
        }
        if self.restart.first().is_none_or(String::is_empty) {
            return Err("restart names no command".to_owned());
        }
        if self.timeout_secs == 0 {
            return Err("timeout_secs is 0".to_owned());
        }
        Ok(())
    }

    /// The instant the deploy must be done by, counted from `start`.
    pub(crate) fn deadline(&self, start: Instant) -> Result<Instant, String> {
        start
            .checked_add(Duration::from_secs(self.timeout_secs))
            .ok_or_else(|| format!("timeout_secs {} is too large", self.timeout_secs))
    }
}

/// Run the restart command from `cwd`, then probe until the endpoint answers
/// with `digest`, all by `deadline`.
pub(crate) fn converge(
    declaration: &Declaration,
    digest: &str,
    cwd: &Path,
    deadline: Instant,
) -> Result<(), String> {
    restart(&declaration.restart, cwd, deadline)?;
    confirm(&declaration.url, digest, deadline).map_err(|last| {
        format!(
            "{} did not answer with this build ({digest}) within {} s of deploy starting; last: {last}",
            declaration.url, declaration.timeout_secs
        )
    })
}

fn restart(argv: &[String], cwd: &Path, deadline: Instant) -> Result<(), String> {
    let shown = argv.join(" ");
    let (program, args) = argv.split_first().ok_or("restart names no command")?;
    // Reading the executable may have used the whole budget; a restart
    // started now could only be killed, after disturbing the service.
    if Instant::now() >= deadline {
        return Err(format!(
            "the deadline passed before the restart command `{shown}` was run"
        ));
    }
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot run the restart command `{shown}`: {e}"))?;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                return Err(format!("the restart command `{shown}` failed: {status}"));
            }
            Ok(None) => {}
            Err(e) => {
                let ended = stop(&mut child);
                return Err(format!(
                    "cannot wait for the restart command `{shown}`: {e}; it was {ended}"
                ));
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let ended = stop(&mut child);
            return Err(format!(
                "the restart command `{shown}` did not finish within the deadline and was {ended}; what it started may still complete"
            ));
        }
        std::thread::sleep(RESTART_POLL.min(remaining));
    }
}

/// Kill a restart command and reap it, saying which happened. A command
/// that cannot be killed is not waited for; the watchdog in [`run`] bounds
/// a reap that blocks.
fn stop(child: &mut std::process::Child) -> &'static str {
    match child.kill() {
        Ok(()) => {
            // Its status says nothing the deadline did not.
            drop(child.wait());
            "killed"
        }
        Err(_) => "left running, since it could not be killed",
    }
}

/// Probe `url` until an answer reports `digest`. The error is the last
/// thing seen when the deadline passed.
fn confirm(url: &str, digest: &str, deadline: Instant) -> Result<(), String> {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        // The declared endpoint, not whatever the environment routes it to.
        .proxy(None)
        .max_idle_connections(0)
        .max_idle_connections_per_host(0)
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let mut backoff = BACKOFF_FIRST;
    let mut last = "no probe was sent".to_owned();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(last);
        }
        match observe(&agent, url, remaining.min(PROBE_TIMEOUT)) {
            Ok(build) if build == digest && Instant::now() < deadline => return Ok(()),
            Ok(build) => last = format!("it answered with build {build}"),
            Err(why) => last = why,
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        std::thread::sleep(backoff.min(remaining));
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// One probe: the build a valid answer made for it reports.
fn observe(agent: &ureq::Agent, url: &str, timeout: Duration) -> Result<String, String> {
    let probe = identity::fresh_probe()?;
    let mut response = agent
        .get(&format!("{url}?probe={probe}"))
        .config()
        .timeout_global(Some(timeout))
        .build()
        .call()
        .map_err(|e| format!("it did not answer: {e}"))?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(format!("it answered status {status}"));
    }
    let body = response
        .body_mut()
        .with_config()
        .limit(ANSWER_LIMIT)
        .read_to_vec()
        .map_err(|e| format!("its answer could not be read: {e}"))?;
    judge(&body, &probe)
}

/// The build an identity answer reports, if it is one made for `probe` by a
/// server of this protocol.
pub(crate) fn judge(body: &[u8], probe: &str) -> Result<String, String> {
    let answer: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| "its answer is not JSON".to_owned())?;
    let field = |name: &str| answer.get(name).and_then(serde_json::Value::as_str);
    if field("server") != Some(SERVER) {
        return Err("its answer is not a brokkr results server's".to_owned());
    }
    let protocol = answer.get("protocol").and_then(serde_json::Value::as_u64);
    if protocol != Some(u64::from(wire::PROTOCOL_VERSION)) {
        return Err(format!(
            "it speaks protocol {}, this build {}",
            protocol.map_or_else(|| "none".to_owned(), |p| p.to_string()),
            wire::PROTOCOL_VERSION
        ));
    }
    if field("probe") != Some(probe) {
        return Err("its answer was not made for this probe".to_owned());
    }
    match field("build") {
        Some(build) if build.len() == 32 && build.bytes().all(|b| b.is_ascii_hexdigit()) => {
            Ok(build.to_owned())
        }
        _ => Err("it reports no build".to_owned()),
    }
}
