//! Sæhrimnir orchestration helpers for plan 3.
//!
//! Sæhrimnir is the mock email-protocol server (plan 2,
//! `/home/folk/Programs/sæhrimnir/`). When brokkr spawns it for sync
//! orchestration the contract is process-level only:
//!
//! - argv: `--fixture <PATH> --readiness-file <PATH>` (plus optional
//!   per-protocol port flags brokkr does not use in v0; ephemeral
//!   ports + sentinel reporting is the simpler path).
//! - sentinel: written atomically (`<NAME> <port>\n` per protocol)
//!   once every listener is bound. Brokkr's existing
//!   `wait_for_sentinel` waits for presence; this module parses the
//!   content into [`Endpoints`].
//! - SIGTERM: clean shutdown within ~1s. SIGKILL is the backstop.
//!
//! Sæhrimnir's repo is authoritative for protocol surface, fixture
//! model, and signal handling; brokkr only orchestrates.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::config::RatatoskrConfig;
use crate::error::DevError;
use crate::output;
use crate::ratatoskr::process as proc_helpers;

/// Resolved per-protocol listening ports parsed out of sæhrimnir's
/// readiness sentinel. Every field is required - sæhrimnir always
/// binds all five listeners and the sentinel always carries one
/// line per protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoints {
    pub jmap: u16,
    pub imap: u16,
    pub smtp: u16,
    pub graph: u16,
    pub gmail: u16,
}

/// Parse sæhrimnir's readiness sentinel. Expects exactly five lines,
/// one per protocol, in any order:
///
/// ```text
/// JMAP <port>
/// IMAP <port>
/// SMTP <port>
/// GRAPH <port>
/// GMAIL <port>
/// ```
///
/// Whitespace between name and port is one-or-more ASCII spaces.
/// Blank lines are tolerated (the file is written atomically via
/// rename, but a future sæhrimnir might emit a trailing newline);
/// duplicate protocols, missing protocols, and unparseable port
/// values all fail loudly so a contract drift gets caught at the
/// orchestration boundary instead of two layers down.
pub fn parse_sentinel(text: &str) -> Result<Endpoints, DevError> {
    let mut jmap: Option<u16> = None;
    let mut imap: Option<u16> = None;
    let mut smtp: Option<u16> = None;
    let mut graph: Option<u16> = None;
    let mut gmail: Option<u16> = None;
    let mut seen: HashSet<&'static str> = HashSet::new();

    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let (name, rest) = line.split_once(char::is_whitespace).ok_or_else(|| {
            DevError::Config(format!(
                "saehrimnir sentinel line {} has no port: {raw:?}",
                lineno + 1
            ))
        })?;
        let port_str = rest.trim();
        let port: u16 = port_str.parse().map_err(|e| {
            DevError::Config(format!(
                "saehrimnir sentinel line {}: port {port_str:?} for {name}: {e}",
                lineno + 1
            ))
        })?;

        let slot: (&'static str, &mut Option<u16>) = match name {
            "JMAP" => ("JMAP", &mut jmap),
            "IMAP" => ("IMAP", &mut imap),
            "SMTP" => ("SMTP", &mut smtp),
            "GRAPH" => ("GRAPH", &mut graph),
            "GMAIL" => ("GMAIL", &mut gmail),
            other => {
                return Err(DevError::Config(format!(
                    "saehrimnir sentinel line {}: unknown protocol {other:?} \
                     (expected JMAP / IMAP / SMTP / GRAPH / GMAIL)",
                    lineno + 1
                )));
            }
        };
        if !seen.insert(slot.0) {
            return Err(DevError::Config(format!(
                "saehrimnir sentinel: duplicate {} entry",
                slot.0
            )));
        }
        *slot.1 = Some(port);
    }

    let missing: Vec<&str> = [
        ("JMAP", jmap.is_none()),
        ("IMAP", imap.is_none()),
        ("SMTP", smtp.is_none()),
        ("GRAPH", graph.is_none()),
        ("GMAIL", gmail.is_none()),
    ]
    .into_iter()
    .filter_map(|(n, missing)| missing.then_some(n))
    .collect();
    if !missing.is_empty() {
        return Err(DevError::Config(format!(
            "saehrimnir sentinel missing entries: {}",
            missing.join(", ")
        )));
    }

    Ok(Endpoints {
        // unwrap is safe: we just verified `missing` is empty.
        jmap: jmap.expect("checked above"),
        imap: imap.expect("checked above"),
        smtp: smtp.expect("checked above"),
        graph: graph.expect("checked above"),
        gmail: gmail.expect("checked above"),
    })
}

// ---------------------------------------------------------------------------
// MockServer: spawn sæhrimnir, wait for the readiness sentinel, hand
// the caller back a handle that owns the child process and the parsed
// endpoints. Used by both `sync-smoke` and `sync-bench` (and indirectly
// by `mock-serve`, which has its own foreground signal-loop layered
// on top).
// ---------------------------------------------------------------------------

/// A spawned sæhrimnir process plus its parsed endpoints. The handle
/// is `Drop`-safe: a panic between spawn and `shutdown` SIGKILLs the
/// child rather than leaking it. The graceful path is
/// [`MockServer::shutdown`] which SIGTERMs first with the standard
/// [`SHUTDOWN_BUDGET`].
pub struct MockServer {
    child: Option<std::process::Child>,
    pid: u32,
    endpoints: Endpoints,
}

/// Diagnostic outcome of [`MockServer::shutdown`]. None of these fields
/// gate caller success - they're recorded into `run.toml` for triage.
pub struct MockOutcome {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub killed_after_budget: bool,
}

impl MockServer {
    /// Spawn sæhrimnir against `fixture_path`, write the readiness
    /// sentinel into `mock_dir/readiness`, and capture stderr to
    /// `mock_dir/stderr.log`. Blocks until the sentinel parses or the
    /// child exits / the readiness budget expires; on those failure
    /// paths, the child is reaped before the error returns so the
    /// caller never sees a zombie.
    pub fn spawn(
        binary: &Path,
        fixture_path: &Path,
        mock_dir: &Path,
    ) -> Result<Self, DevError> {
        let readiness = mock_dir.join("readiness");
        if readiness.exists() {
            std::fs::remove_file(&readiness).map_err(DevError::Io)?;
        }
        let stderr_log =
            std::fs::File::create(mock_dir.join("stderr.log")).map_err(DevError::Io)?;

        let fixture_str = fixture_path.display().to_string();
        let readiness_str = readiness.display().to_string();
        let mut child = Command::new(binary)
            .args([
                "--fixture",
                &fixture_str,
                "--readiness-file",
                &readiness_str,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr_log)
            .spawn()
            .map_err(|e| DevError::Subprocess {
                program: binary.display().to_string(),
                code: None,
                stderr: format!("failed to spawn: {e}"),
            })?;
        let pid = child.id();

        let endpoints = match wait_for_endpoints(&mut child, &readiness, READINESS_BUDGET) {
            Ok(ep) => ep,
            Err(e) => {
                let _term = proc_helpers::send_signal(pid, libc::SIGTERM);
                let _reaped = child.wait();
                return Err(e);
            }
        };

        Ok(Self {
            child: Some(child),
            pid,
            endpoints,
        })
    }

    pub fn endpoints(&self) -> &Endpoints {
        &self.endpoints
    }

    /// SIGTERM the child, grant [`SHUTDOWN_BUDGET`], escalate to SIGKILL
    /// on timeout. Always reaps. Consumes the handle so the Drop fallback
    /// can't double-fire.
    pub fn shutdown(mut self) -> MockOutcome {
        use std::os::unix::process::ExitStatusExt;

        let Some(mut child) = self.child.take() else {
            return MockOutcome {
                exit_code: None,
                signal: None,
                killed_after_budget: false,
            };
        };
        let _term = proc_helpers::send_signal(self.pid, libc::SIGTERM);
        match wait_with_deadline(&mut child, self.pid, SHUTDOWN_BUDGET) {
            Ok(status) => MockOutcome {
                exit_code: status.code(),
                signal: status.signal(),
                killed_after_budget: matches!(status.signal(), Some(s) if s == libc::SIGKILL),
            },
            Err(_) => MockOutcome {
                exit_code: None,
                signal: None,
                killed_after_budget: true,
            },
        }
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        // Only fires when the caller didn't go through `shutdown` -
        // typically a panic mid-orchestration. Hard-kill rather than
        // leak the child; nothing graceful to wait for if we're
        // unwinding.
        if let Some(mut child) = self.child.take() {
            let _kill = proc_helpers::send_signal(self.pid, libc::SIGKILL);
            let _reaped = child.wait();
        }
    }
}

// ---------------------------------------------------------------------------
// `brokkr mock-serve`: foreground spawn of sæhrimnir, print endpoints,
// run until ctrl-C.
// ---------------------------------------------------------------------------

/// Where the readiness sentinel lives for `mock-serve`, relative to the
/// project root. Stable across runs so `brokkr clean` / manual
/// inspection both have a known path. `sync-smoke` puts its sentinel
/// inside the per-run artefact dir instead.
const MOCK_DIR: &str = ".brokkr/ratatoskr/mock";

/// Wall-clock budget for sæhrimnir to bind all five listeners and write
/// the readiness sentinel. Generous - sæhrimnir's cold-start is sub-100ms
/// in practice, but the budget needs to absorb a debug-build startup or
/// a CPU-pinned host without spurious failures.
pub const READINESS_BUDGET: Duration = Duration::from_secs(10);

/// Wall-clock budget for sæhrimnir to honour SIGTERM. Plan 2 promises
/// "clean shutdown within 1s"; we give it 1.5 to absorb scheduler jitter
/// before escalating to SIGKILL.
pub const SHUTDOWN_BUDGET: Duration = Duration::from_millis(1500);

/// Resolved inputs for `brokkr mock-serve`. Pulled out of the
/// CLI-dispatch shell so the spawn-and-loop body can be unit-tested
/// against synthetic paths if desired.
pub struct MockServeRequest<'a> {
    pub project_root: &'a Path,
    pub config: &'a RatatoskrConfig,
    /// Fixture name from the CLI argument. Resolves against
    /// `<fixtures_dir>/<name>.{toml,lua}` (whichever exists).
    pub fixture: &'a str,
}

/// Drive `brokkr mock-serve` end-to-end: validate config, resolve fixture,
/// spawn sæhrimnir, wait for the readiness sentinel, print endpoints,
/// then loop until the user signals or the child exits. SIGTERMs the
/// child on signal with a [`SHUTDOWN_BUDGET`] before escalating to
/// SIGKILL.
pub fn run_mock_serve(req: &MockServeRequest<'_>) -> Result<(), DevError> {
    let binary = require_path(
        &req.config.mock_server_binary,
        req.project_root,
        "mock_server_binary",
    )?;
    let fixtures_dir = require_path(
        &req.config.fixtures_dir,
        req.project_root,
        "fixtures_dir",
    )?;
    let fixture_path = resolve_fixture(&fixtures_dir, req.fixture)?;

    if !binary.exists() {
        return Err(DevError::Config(format!(
            "mock-serve: sæhrimnir binary not found at {}. \
             Build it first: `cargo build --release` in sæhrimnir's repo. \
             Auto-build is not yet wired (plan 3 follow-up).",
            binary.display()
        )));
    }

    let mock_dir = req.project_root.join(MOCK_DIR);
    std::fs::create_dir_all(&mock_dir).map_err(DevError::Io)?;
    let readiness = mock_dir.join("readiness");
    // Stale sentinel from a prior crashed run would fool wait_for_sentinel
    // into resolving immediately with the wrong ports.
    if readiness.exists() {
        std::fs::remove_file(&readiness).map_err(DevError::Io)?;
    }

    let readiness_str = readiness.display().to_string();
    let fixture_str = fixture_path.display().to_string();
    output::ratatoskr_msg(&format!(
        "spawning {} (fixture: {})",
        binary.display(),
        fixture_str
    ));

    let mut child = Command::new(&binary)
        .args(["--fixture", &fixture_str, "--readiness-file", &readiness_str])
        .stdin(Stdio::null())
        // Inherit stdout/stderr so the user sees sæhrimnir's logs live.
        // Capture-and-replay would obscure timing for a foreground tool.
        .spawn()
        .map_err(|e| DevError::Subprocess {
            program: binary.display().to_string(),
            code: None,
            stderr: format!("failed to spawn: {e}"),
        })?;
    let child_pid = child.id();

    let endpoints = match wait_for_endpoints(&mut child, &readiness, READINESS_BUDGET) {
        Ok(ep) => ep,
        Err(e) => {
            // Best-effort cleanup if sæhrimnir spawned but never wrote
            // the sentinel - don't leave it running in the background.
            let _term = proc_helpers::send_signal(child_pid, libc::SIGTERM);
            let _reaped = child.wait();
            return Err(e);
        }
    };

    print_endpoints(&endpoints);
    output::ratatoskr_msg("press Ctrl-C to stop");

    // Install SIGINT + SIGTERM handlers scoped to this run. Default
    // handlers would kill brokkr immediately and orphan the child;
    // intercepting lets us drain the child within the SHUTDOWN_BUDGET.
    let _signals = MockServeSignalGuard::install();

    let exit_status = wait_with_signals(&mut child, child_pid)?;

    if let Some(code) = exit_status.code() {
        if code != 0 {
            output::ratatoskr_msg(&format!("saehrimnir exited with code {code}"));
            return Err(DevError::ExitCode(code));
        }
        output::ratatoskr_msg("saehrimnir exited cleanly");
    } else {
        // Killed by signal. If WE sent SIGTERM (graceful path), that's fine.
        // If something else killed it, surface it.
        use std::os::unix::process::ExitStatusExt;
        let signal_label = exit_status
            .signal()
            .map(|s| format!("signal {s}"))
            .unwrap_or_else(|| "unknown".into());
        output::ratatoskr_msg(&format!("saehrimnir terminated by {signal_label}"));
    }
    Ok(())
}

/// Resolve an `Option<PathBuf>` config field that's required for this
/// command, joining relative paths against `<project_root>` so the
/// caller's cwd doesn't matter.
pub fn require_path(
    value: &Option<PathBuf>,
    project_root: &Path,
    field_name: &str,
) -> Result<PathBuf, DevError> {
    let raw = value.as_ref().ok_or_else(|| {
        DevError::Config(format!(
            "[ratatoskr] {field_name} is not set in brokkr.toml. \
             Required to locate sæhrimnir's binary and fixtures."
        ))
    })?;
    Ok(if raw.is_absolute() {
        raw.clone()
    } else {
        project_root.join(raw)
    })
}

/// Pick the right file for a fixture name. `.toml` and `.lua` are both
/// valid sæhrimnir fixture formats; the loader dispatches by extension.
/// If the name already carries one of those extensions, take it
/// literally - that's the disambiguation hatch when both
/// `<stem>.toml` and `<stem>.lua` coexist. Bare stems pick whichever
/// file exists, and refuse if both do (the user almost certainly has a
/// stale copy in the wrong format).
pub fn resolve_fixture(fixtures_dir: &Path, name: &str) -> Result<PathBuf, DevError> {
    if name.ends_with(".toml") || name.ends_with(".lua") {
        let path = fixtures_dir.join(name);
        return if path.exists() {
            Ok(path)
        } else {
            Err(DevError::Config(format!(
                "fixture {name:?} not found - looked for {}.",
                path.display()
            )))
        };
    }
    let toml_path = fixtures_dir.join(format!("{name}.toml"));
    let lua_path = fixtures_dir.join(format!("{name}.lua"));
    match (toml_path.exists(), lua_path.exists()) {
        (true, true) => Err(DevError::Config(format!(
            "fixture {name:?} ambiguous - both {} and {} exist. \
             Disambiguate by writing the fixture name with its extension \
             (e.g. {name:?}.toml or {name:?}.lua).",
            toml_path.display(),
            lua_path.display()
        ))),
        (true, false) => Ok(toml_path),
        (false, true) => Ok(lua_path),
        (false, false) => Err(DevError::Config(format!(
            "fixture {name:?} not found - looked for {} and {}.",
            toml_path.display(),
            lua_path.display()
        ))),
    }
}

/// Poll for the readiness sentinel while keeping the child alive.
/// Returns the parsed [`Endpoints`] on success; errors if the child
/// exits before writing the sentinel (covers fixture-validation errors,
/// port-in-use, etc.) or if the budget expires.
pub fn wait_for_endpoints(
    child: &mut std::process::Child,
    readiness: &Path,
    budget: Duration,
) -> Result<Endpoints, DevError> {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().map_err(DevError::Io)? {
            return Err(DevError::Config(format!(
                "saehrimnir exited before writing the readiness sentinel \
                 (status: {status:?}). Check stderr above for the failure."
            )));
        }
        if readiness.exists() {
            let text = std::fs::read_to_string(readiness).map_err(DevError::Io)?;
            return parse_sentinel(&text);
        }
        if started.elapsed() > budget {
            return Err(DevError::Config(format!(
                "saehrimnir did not write a readiness sentinel within {budget:?}."
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Print the per-protocol endpoint table for a manual session. URL
/// shapes match what ratatoskr's existing client code expects: HTTP
/// origins for the JSON-over-HTTP protocols, `host:port` for the
/// stream protocols.
fn print_endpoints(ep: &Endpoints) {
    output::ratatoskr_msg("listening on:");
    output::ratatoskr_msg(&format!("  jmap   http://127.0.0.1:{}", ep.jmap));
    output::ratatoskr_msg(&format!("  imap   127.0.0.1:{}", ep.imap));
    output::ratatoskr_msg(&format!("  smtp   127.0.0.1:{}", ep.smtp));
    output::ratatoskr_msg(&format!("  graph  http://127.0.0.1:{}", ep.graph));
    output::ratatoskr_msg(&format!("  gmail  http://127.0.0.1:{}", ep.gmail));
}

/// Wait for the child while watching the SIGINT/SIGTERM flag. On signal
/// we send SIGTERM and grant [`SHUTDOWN_BUDGET`] before SIGKILL; on
/// child-exit we just reap.
fn wait_with_signals(
    child: &mut std::process::Child,
    pid: u32,
) -> Result<std::process::ExitStatus, DevError> {
    loop {
        if let Some(status) = child.try_wait().map_err(DevError::Io)? {
            return Ok(status);
        }
        if MOCK_SIGNAL_FLAG.load(Ordering::Relaxed) {
            output::ratatoskr_msg("signal received, shutting saehrimnir down");
            // Best-effort SIGTERM; if the child has just exited, ESRCH
            // is fine.
            let _term = proc_helpers::send_signal(pid, libc::SIGTERM);
            return wait_with_deadline(child, pid, SHUTDOWN_BUDGET);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// After SIGTERM, give the child up to `budget` to exit; SIGKILL if it
/// outlives that. SIGKILL still requires a final `wait` to reap, which
/// we always perform so the caller never sees a zombie PID.
pub fn wait_with_deadline(
    child: &mut std::process::Child,
    pid: u32,
    budget: Duration,
) -> Result<std::process::ExitStatus, DevError> {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().map_err(DevError::Io)? {
            return Ok(status);
        }
        if started.elapsed() > budget {
            output::ratatoskr_msg(&format!(
                "saehrimnir did not exit within {budget:?}, escalating to SIGKILL"
            ));
            let _kill = proc_helpers::send_signal(pid, libc::SIGKILL);
            return child.wait().map_err(DevError::Io);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

// ---------------------------------------------------------------------------
// Signal handling for mock-serve. Scoped via RAII guard so non-mock-serve
// code keeps its default SIGINT/SIGTERM behaviour. Separate atomic from
// `crate::shutdown::SHUTDOWN_REQUESTED` so the global `brokkr kill` flow
// (which expects a sidecar polling that flag) and this command don't
// interfere with each other.
// ---------------------------------------------------------------------------

static MOCK_SIGNAL_FLAG: AtomicBool = AtomicBool::new(false);

extern "C" fn mock_signal_handler(_: libc::c_int) {
    MOCK_SIGNAL_FLAG.store(true, Ordering::Relaxed);
}

fn set_handler(sig: libc::c_int, handler: libc::sighandler_t) {
    // SAFETY: handler body only writes an AtomicBool, which is
    // async-signal-safe. Function pointer cast matches libc::signal's
    // expected shape.
    unsafe {
        libc::signal(sig, handler);
    }
}

struct MockServeSignalGuard;

impl MockServeSignalGuard {
    fn install() -> Self {
        MOCK_SIGNAL_FLAG.store(false, Ordering::Relaxed);
        let h: libc::sighandler_t = mock_signal_handler as *const () as usize;
        set_handler(libc::SIGINT, h);
        set_handler(libc::SIGTERM, h);
        Self
    }
}

impl Drop for MockServeSignalGuard {
    fn drop(&mut self) {
        set_handler(libc::SIGINT, libc::SIG_DFL);
        set_handler(libc::SIGTERM, libc::SIG_DFL);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn parses_canonical_sentinel() {
        let text = "JMAP 12345\nIMAP 23456\nSMTP 34567\nGRAPH 45678\nGMAIL 56789\n";
        let ep = parse_sentinel(text).unwrap();
        assert_eq!(
            ep,
            Endpoints {
                jmap: 12345,
                imap: 23456,
                smtp: 34567,
                graph: 45678,
                gmail: 56789,
            }
        );
    }

    #[test]
    fn tolerates_blank_lines_and_arbitrary_order() {
        let text = "\nGMAIL 5\n\nGRAPH 4\nJMAP 1\nIMAP 2\nSMTP 3\n";
        let ep = parse_sentinel(text).unwrap();
        assert_eq!(ep.jmap, 1);
        assert_eq!(ep.imap, 2);
        assert_eq!(ep.smtp, 3);
        assert_eq!(ep.graph, 4);
        assert_eq!(ep.gmail, 5);
    }

    #[test]
    fn missing_entry_errors_with_named_protocols() {
        let text = "JMAP 1\nIMAP 2\nGMAIL 3\n";
        let err = parse_sentinel(text).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("missing entries"), "got: {msg}");
        assert!(msg.contains("SMTP"), "got: {msg}");
        assert!(msg.contains("GRAPH"), "got: {msg}");
    }

    #[test]
    fn duplicate_entry_errors() {
        let text = "JMAP 1\nIMAP 2\nSMTP 3\nGRAPH 4\nGMAIL 5\nJMAP 6\n";
        let err = parse_sentinel(text).unwrap_err();
        assert!(err.to_string().contains("duplicate JMAP"), "got: {err}");
    }

    #[test]
    fn unknown_protocol_errors() {
        let text = "JMAP 1\nXMPP 9\n";
        let err = parse_sentinel(text).unwrap_err();
        assert!(err.to_string().contains("unknown protocol"), "got: {err}");
    }

    #[test]
    fn unparseable_port_errors() {
        let text = "JMAP not-a-number\n";
        let err = parse_sentinel(text).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("port"), "got: {msg}");
        assert!(msg.contains("JMAP"), "got: {msg}");
    }

    #[test]
    fn port_out_of_range_errors() {
        // u16 max is 65535; 70000 won't fit.
        let text = "JMAP 70000\n";
        let err = parse_sentinel(text).unwrap_err();
        assert!(err.to_string().contains("port"), "got: {err}");
    }

    #[test]
    fn line_with_only_a_name_errors() {
        let text = "JMAP\n";
        let err = parse_sentinel(text).unwrap_err();
        assert!(err.to_string().contains("no port"), "got: {err}");
    }

    fn fixture_tmpdir(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-tmp/mock-serve-fixture")
            .join(name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn resolve_fixture_picks_toml_when_only_toml() {
        let dir = fixture_tmpdir("toml_only");
        std::fs::write(dir.join("alpha.toml"), "").unwrap();
        let resolved = resolve_fixture(&dir, "alpha").unwrap();
        assert_eq!(resolved, dir.join("alpha.toml"));
    }

    #[test]
    fn resolve_fixture_picks_lua_when_only_lua() {
        let dir = fixture_tmpdir("lua_only");
        std::fs::write(dir.join("beta.lua"), "").unwrap();
        let resolved = resolve_fixture(&dir, "beta").unwrap();
        assert_eq!(resolved, dir.join("beta.lua"));
    }

    #[test]
    fn resolve_fixture_errors_when_both_extensions_present() {
        let dir = fixture_tmpdir("both");
        std::fs::write(dir.join("gamma.toml"), "").unwrap();
        std::fs::write(dir.join("gamma.lua"), "").unwrap();
        let err = resolve_fixture(&dir, "gamma").unwrap_err();
        assert!(err.to_string().contains("ambiguous"), "got: {err}");
    }

    #[test]
    fn resolve_fixture_explicit_extension_disambiguates() {
        let dir = fixture_tmpdir("explicit_ext");
        std::fs::write(dir.join("delta.toml"), "").unwrap();
        std::fs::write(dir.join("delta.lua"), "").unwrap();
        assert_eq!(
            resolve_fixture(&dir, "delta.toml").unwrap(),
            dir.join("delta.toml")
        );
        assert_eq!(
            resolve_fixture(&dir, "delta.lua").unwrap(),
            dir.join("delta.lua")
        );
    }

    #[test]
    fn resolve_fixture_explicit_extension_missing_errors() {
        let dir = fixture_tmpdir("explicit_ext_missing");
        std::fs::write(dir.join("epsilon.toml"), "").unwrap();
        let err = resolve_fixture(&dir, "epsilon.lua").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not found"), "got: {msg}");
        assert!(msg.contains("epsilon.lua"), "got: {msg}");
    }

    #[test]
    fn resolve_fixture_errors_when_missing() {
        let dir = fixture_tmpdir("missing");
        let err = resolve_fixture(&dir, "ghost").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not found"), "got: {msg}");
        assert!(msg.contains("ghost"), "got: {msg}");
    }

    #[test]
    fn require_path_joins_relative_against_root() {
        let value = Some(PathBuf::from("rel/path"));
        let root = Path::new("/proj");
        let resolved = require_path(&value, root, "fixtures_dir").unwrap();
        assert_eq!(resolved, PathBuf::from("/proj/rel/path"));
    }

    #[test]
    fn require_path_keeps_absolute_path() {
        let value = Some(PathBuf::from("/etc/foo"));
        let root = Path::new("/proj");
        let resolved = require_path(&value, root, "fixtures_dir").unwrap();
        assert_eq!(resolved, PathBuf::from("/etc/foo"));
    }

    #[test]
    fn require_path_errors_when_unset() {
        let value: Option<PathBuf> = None;
        let root = Path::new("/proj");
        let err = require_path(&value, root, "fixtures_dir").unwrap_err();
        assert!(err.to_string().contains("fixtures_dir"), "got: {err}");
    }
}
