//! Sæhrimnir orchestration helpers for service and sync runs.
//!
//! Sæhrimnir is an external mock email-protocol server. When brokkr spawns
//! it for service or sync orchestration the contract is process-level only:
//!
//! - argv: `--fixture <PATH> --readiness-file <PATH>` (plus optional
//!   per-protocol port flags brokkr does not use in v0; ephemeral
//!   ports + sentinel reporting is the simpler path).
//! - sentinel: written atomically (`<NAME> <port>\n` per protocol)
//!   once every listener is bound. [`wait_for_endpoints`] waits for the
//!   sentinel and parses its content into [`Endpoints`].
//! - SIGTERM: clean shutdown within ~1s. SIGKILL is the backstop.
//!
//! Sæhrimnir's repo is authoritative for protocol surface, fixture
//! model, and signal handling; brokkr only orchestrates.

use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::clean::layout::ProjectState;
use crate::config::project_config::ratatoskr::RatatoskrConfig;
use crate::error::{DevError, ProcessEnding};
use crate::lockfile::LockGuard;
use crate::output;
use crate::ratatoskr::resolve_config_path;
use crate::ratatoskr::timing::{POLL_INTERVAL, SHUTDOWN_POLL_INTERVAL};
pub use crate::ratatoskr::timing::{READINESS_BUDGET, SHUTDOWN_BUDGET};

const MOCK_HOST: &str = "127.0.0.1";

/// A protocol exposed by sæhrimnir's readiness sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum Protocol {
    Jmap,
    Imap,
    Smtp,
    Graph,
    Gmail,
    Caldav,
    Carddav,
    People,
    Gcal,
    #[doc(hidden)]
    __Count,
}

impl Protocol {
    pub(crate) const COUNT: usize = Self::__Count as usize;
    const ALL: [Self; Self::COUNT] = [
        Self::Jmap,
        Self::Imap,
        Self::Smtp,
        Self::Graph,
        Self::Gmail,
        Self::Caldav,
        Self::Carddav,
        Self::People,
        Self::Gcal,
    ];

    fn index(self) -> usize {
        self as usize
    }

    pub(crate) fn all() -> impl Iterator<Item = Self> {
        Self::ALL.into_iter()
    }

    fn from_sentinel(name: &str) -> Option<Self> {
        Self::all().find(|protocol| protocol.sentinel_name() == name)
    }

    pub(crate) fn name(self) -> &'static str {
        self.spec().name
    }

    fn sentinel_name(self) -> &'static str {
        self.spec().sentinel_name
    }

    fn configured_env(self, cfg: &RatatoskrConfig) -> Option<&str> {
        (self.spec().endpoint_env)(cfg)
    }

    fn endpoint(self, port: u16) -> String {
        match self.spec().format {
            EndpointFormat::HttpOrigin => format!("http://{MOCK_HOST}:{port}"),
            EndpointFormat::HostPort => format!("{MOCK_HOST}:{port}"),
        }
    }
}

pub(crate) fn is_protocol_name(name: &str) -> bool {
    // `discovery` is a harness annotation for WebFinger tests; sæhrimnir
    // serves it through JMAP and does not expose a separate listener.
    name == "discovery" || Protocol::all().any(|protocol| protocol.name() == name)
}

/// Validated sæhrimnir locations shared by service and sync preflight.
#[derive(Debug, Clone)]
pub struct MockPaths {
    pub binary: PathBuf,
    pub fixtures_dir: PathBuf,
}

impl MockPaths {
    pub fn resolve(cfg: &RatatoskrConfig, project_root: &Path) -> Result<Self, DevError> {
        let binary = require_path(&cfg.mock_server_binary, project_root, "mock_server_binary")?;
        if !binary.is_file() {
            return Err(DevError::Config(format!(
                "sæhrimnir binary is not a file: {}",
                binary.display()
            )));
        }
        let fixtures_dir = require_path(&cfg.fixtures_dir, project_root, "fixtures_dir")?;
        if !fixtures_dir.is_dir() {
            return Err(DevError::Config(format!(
                "fixtures_dir is not a directory: {}",
                fixtures_dir.display()
            )));
        }
        Ok(Self {
            binary,
            fixtures_dir,
        })
    }

    pub fn fixture(&self, name: &str) -> Result<PathBuf, DevError> {
        resolve_fixture(&self.fixtures_dir, name)
    }
}

/// The common artefact placement rules for mock lifecycles. Service shares
/// one capture directory per fixture; sync nests it under each run record.
#[derive(Clone, Copy)]
pub enum MockArtifactScope<'a> {
    Run(&'a Path),
    SharedFixture {
        project_root: &'a Path,
        fixture_name: &'a str,
    },
}

impl MockArtifactScope<'_> {
    fn directory(&self) -> PathBuf {
        match self {
            Self::Run(run_dir) => run_dir.join("mock"),
            Self::SharedFixture {
                project_root,
                fixture_name,
            } => ProjectState::new(project_root)
                .ratatoskr()
                .service_mock_artifacts()
                .join(safe_fixture_dir_name(fixture_name)),
        }
    }
}

fn safe_fixture_dir_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[derive(Clone, Copy)]
enum EndpointFormat {
    HttpOrigin,
    HostPort,
}

type EndpointEnvLookup = for<'a> fn(&'a RatatoskrConfig) -> Option<&'a str>;

struct ProtocolSpec {
    name: &'static str,
    sentinel_name: &'static str,
    format: EndpointFormat,
    endpoint_env: EndpointEnvLookup,
}

// This exhaustive match is the protocol registry. A new variant must
// supply its sentinel name, typed config accessor and endpoint shape; the enum's
// `all` iterator then drives parsing and output consistently.
impl Protocol {
    fn spec(self) -> ProtocolSpec {
        match self {
            Self::Jmap => ProtocolSpec {
                name: "jmap",
                sentinel_name: "JMAP",
                format: EndpointFormat::HttpOrigin,
                endpoint_env: |cfg| cfg.test_endpoint_env_jmap.as_deref(),
            },
            Self::Imap => ProtocolSpec {
                name: "imap",
                sentinel_name: "IMAP",
                format: EndpointFormat::HostPort,
                endpoint_env: |cfg| cfg.test_endpoint_env_imap.as_deref(),
            },
            Self::Smtp => ProtocolSpec {
                name: "smtp",
                sentinel_name: "SMTP",
                format: EndpointFormat::HostPort,
                endpoint_env: |cfg| cfg.test_endpoint_env_smtp.as_deref(),
            },
            Self::Graph => ProtocolSpec {
                name: "graph",
                sentinel_name: "GRAPH",
                format: EndpointFormat::HttpOrigin,
                endpoint_env: |cfg| cfg.test_endpoint_env_graph.as_deref(),
            },
            Self::Gmail => ProtocolSpec {
                name: "gmail",
                sentinel_name: "GMAIL",
                format: EndpointFormat::HttpOrigin,
                endpoint_env: |cfg| cfg.test_endpoint_env_gmail.as_deref(),
            },
            Self::Caldav => ProtocolSpec {
                name: "caldav",
                sentinel_name: "CALDAV",
                format: EndpointFormat::HttpOrigin,
                endpoint_env: |cfg| cfg.test_endpoint_env_caldav.as_deref(),
            },
            Self::Carddav => ProtocolSpec {
                name: "carddav",
                sentinel_name: "CARDDAV",
                format: EndpointFormat::HttpOrigin,
                endpoint_env: |cfg| cfg.test_endpoint_env_carddav.as_deref(),
            },
            Self::People => ProtocolSpec {
                name: "people",
                sentinel_name: "PEOPLE",
                format: EndpointFormat::HttpOrigin,
                endpoint_env: |cfg| cfg.test_endpoint_env_people.as_deref(),
            },
            Self::Gcal => ProtocolSpec {
                name: "gcal",
                sentinel_name: "GCAL",
                format: EndpointFormat::HttpOrigin,
                endpoint_env: |cfg| cfg.test_endpoint_env_gcal.as_deref(),
            },
            Self::__Count => unreachable!("protocol count marker has no metadata"),
        }
    }
}

/// Resolved per-protocol listening ports parsed out of sæhrimnir's
/// readiness sentinel. Every entry is required - sæhrimnir always binds
/// all nine listeners and the sentinel carries one line per protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoints {
    ports: [u16; Protocol::COUNT],
}

impl Endpoints {
    /// Construct endpoints in the order returned by [`Protocol::all`].
    pub(crate) const fn from_ordered(ports: [u16; Protocol::COUNT]) -> Self {
        Self { ports }
    }

    pub(crate) fn port(&self, protocol: Protocol) -> u16 {
        self.ports[protocol.index()]
    }
}

/// Parse sæhrimnir's readiness sentinel. Expects exactly nine lines,
/// one per protocol, in any order:
///
/// ```text
/// JMAP <port>
/// IMAP <port>
/// SMTP <port>
/// GRAPH <port>
/// GMAIL <port>
/// CALDAV <port>
/// CARDDAV <port>
/// PEOPLE <port>
/// GCAL <port>
/// ```
///
/// Whitespace between name and port is one-or-more ASCII spaces.
/// Blank lines are tolerated (the file is written atomically via
/// rename, but a future sæhrimnir might emit a trailing newline);
/// duplicate protocols, missing protocols, and unparseable port
/// values all fail loudly so a contract drift gets caught at the
/// orchestration boundary instead of two layers down.
pub fn parse_sentinel(text: &str) -> Result<Endpoints, DevError> {
    let mut ports = [None; Protocol::COUNT];

    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let (name, rest) = line.split_once(char::is_whitespace).ok_or_else(|| {
            DevError::Verify(format!(
                "saehrimnir sentinel line {} has no port: {raw:?}",
                lineno + 1
            ))
        })?;
        let port_str = rest.trim();
        let port: u16 = port_str.parse().map_err(|e| {
            DevError::Verify(format!(
                "saehrimnir sentinel line {}: port {port_str:?} for {name}: {e}",
                lineno + 1
            ))
        })?;

        let Some(protocol) = Protocol::from_sentinel(name) else {
            let expected = Protocol::all()
                .map(Protocol::sentinel_name)
                .collect::<Vec<_>>()
                .join(" / ");
            return Err(DevError::Verify(format!(
                "saehrimnir sentinel line {}: unknown protocol {name:?} (expected {expected})",
                lineno + 1
            )));
        };
        let slot = &mut ports[protocol.index()];
        if slot.is_some() {
            return Err(DevError::Verify(format!(
                "saehrimnir sentinel: duplicate {} entry",
                protocol.sentinel_name()
            )));
        }
        *slot = Some(port);
    }

    let missing: Vec<&str> = Protocol::all()
        .filter(|protocol| ports[protocol.index()].is_none())
        .map(Protocol::sentinel_name)
        .collect();
    if !missing.is_empty() {
        return Err(DevError::Verify(format!(
            "saehrimnir sentinel missing entries: {}",
            missing.join(", ")
        )));
    }

    // The missing list above proves each slot is Some.
    Ok(Endpoints::from_ordered(
        ports.map(|port| port.expect("checked above")),
    ))
}

// ---------------------------------------------------------------------------
// MockServer: spawn sæhrimnir, wait for the readiness sentinel, hand
// the caller back a handle that owns the child process and the parsed
// endpoints. Owned through the shared service/sync lifecycle below.
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
    ready_elapsed: Duration,
}

/// Diagnostic outcome of [`MockServer::shutdown`]. The exit/signal fields
/// are recorded into `run.toml` for triage; `shutdown_elapsed` is also
/// surfaced in the user-visible per-phase summary.
pub struct MockOutcome {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub killed_after_budget: bool,
    pub shutdown_elapsed: Duration,
}

/// Result of waiting for a child through a SIGTERM grace period.
#[derive(Debug)]
pub struct DeadlineWait {
    pub status: std::process::ExitStatus,
    /// True only when this wait sent SIGKILL after the budget expired.
    pub killed_after_budget: bool,
}

/// One sæhrimnir lifecycle shared by service and sync runs. Callers choose
/// where the mock artefacts live; process-group isolation, PID registration,
/// endpoint export, and teardown follow the same policy everywhere.
pub struct ManagedMock<'a> {
    fixture_name: String,
    server: Option<MockServer>,
    endpoint_envs: Vec<(String, String)>,
    lock: &'a LockGuard,
}

impl<'a> ManagedMock<'a> {
    pub fn start(
        cfg: &RatatoskrConfig,
        binary: &Path,
        fixture_path: &Path,
        fixture_name: &str,
        artifact_scope: MockArtifactScope<'_>,
        lock: &'a LockGuard,
    ) -> Result<Self, DevError> {
        let mock_dir = artifact_scope.directory();
        std::fs::create_dir_all(&mock_dir).map_err(DevError::Io)?;
        let server = MockServer::spawn_observed(
            binary,
            fixture_path,
            &mock_dir,
            Some(&|pid| lock.add_mock_pid(pid)),
            Some(&|pid| lock.remove_mock_pid(pid)),
        )?;
        let endpoint_envs = endpoint_env_pairs(cfg, server.endpoints());
        Ok(Self {
            fixture_name: fixture_name.to_owned(),
            server: Some(server),
            endpoint_envs,
            lock,
        })
    }

    pub fn endpoints(&self) -> &Endpoints {
        self.server
            .as_ref()
            .expect("managed mock is running until shutdown")
            .endpoints()
    }

    pub fn ready_elapsed(&self) -> Duration {
        self.server
            .as_ref()
            .expect("managed mock is running until shutdown")
            .ready_elapsed()
    }

    pub fn env_pair_refs(&self) -> Vec<(&str, &str)> {
        self.endpoint_envs
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect()
    }

    pub fn shutdown(mut self) -> Result<MockOutcome, DevError> {
        let server = self
            .server
            .take()
            .ok_or_else(|| DevError::Internal("managed mock shut down twice".into()))?;
        let pid = server.pid();
        let result = server.shutdown();
        self.lock.remove_mock_pid(pid);
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(error) => {
                output::ratatoskr_msg(&format!(
                    "mock-server shutdown failed (fixture={}): {error}",
                    self.fixture_name
                ));
                return Err(error);
            }
        };
        if outcome.killed_after_budget {
            output::ratatoskr_msg(&format!(
                "mock-server SIGKILLed (fixture={}, drain budget exceeded)",
                self.fixture_name
            ));
        }
        Ok(outcome)
    }
}

impl Drop for ManagedMock<'_> {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            let pid = server.pid();
            drop(server);
            self.lock.remove_mock_pid(pid);
        }
    }
}

impl MockServer {
    /// Spawn sæhrimnir against `fixture_path`, write the readiness
    /// sentinel into `mock_dir/readiness`, and capture stderr to
    /// `mock_dir/stderr.log`. Blocks until the sentinel parses or the
    /// child exits / the readiness budget expires; on those failure
    /// paths, the child is reaped before the error returns so the
    /// caller never sees a zombie.
    ///
    /// `on_spawn` is invoked with the child's PID immediately after
    /// `Command::spawn` returns - BEFORE the readiness wait. Lets the
    /// orchestrator publish the mock PID into the lockfile in time for
    /// `brokkr kill --hard` arriving during the readiness window to
    /// find it.
    ///
    /// `on_failure` is the inverse: invoked with the same PID if
    /// readiness fails (sentinel never written, child exited early,
    /// budget expired). Lets the orchestrator remove the PID it just
    /// published via `on_spawn` immediately.
    pub fn spawn_observed(
        binary: &Path,
        fixture_path: &Path,
        mock_dir: &Path,
        on_spawn: Option<&dyn Fn(u32)>,
        on_failure: Option<&dyn Fn(u32)>,
    ) -> Result<Self, DevError> {
        let readiness = mock_dir.join("readiness");
        if readiness.exists() {
            std::fs::remove_file(&readiness).map_err(DevError::Io)?;
        }
        let stderr_log =
            std::fs::File::create(mock_dir.join("stderr.log")).map_err(DevError::Io)?;

        let fixture_str = fixture_path.display().to_string();
        let readiness_str = readiness.display().to_string();
        let spawn_start = Instant::now();
        // Every managed mock owns a process group. Service and sync callers
        // install a signal guard before spawning, so cooperative signals use
        // the same group-aware cleanup path everywhere.
        use std::os::unix::process::CommandExt;
        let mut cmd = Command::new(binary);
        cmd.args([
            "--fixture",
            &fixture_str,
            "--readiness-file",
            &readiness_str,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr_log);
        cmd.process_group(0);
        // Every child of a hold carries its capability: see `crate::hold`.
        crate::hold::stamp(&mut cmd);
        let mut child = cmd.spawn().map_err(|error| DevError::Spawn {
            program: binary.display().to_string(),
            error,
        })?;
        let pid = child.id();
        if let Some(cb) = on_spawn {
            cb(pid);
        }

        let endpoints = match wait_for_endpoints(&mut child, &readiness, READINESS_BUDGET) {
            Ok(ep) => ep,
            Err(e) => {
                // Bounded shutdown: SIGTERM the process group, give
                // SHUTDOWN_BUDGET, then SIGKILL. A
                // wedged sæhrimnir that ignores SIGTERM can't make the
                // readiness-failure path hang forever.
                kill_for_shutdown(pid, libc::SIGTERM);
                if let Err(cleanup_error) = wait_with_deadline(&mut child, pid, SHUTDOWN_BUDGET) {
                    output::ratatoskr_msg(&format!(
                        "mock-server cleanup also failed after readiness error: {cleanup_error}"
                    ));
                }
                if let Some(cb) = on_failure {
                    cb(pid);
                }
                return Err(e);
            }
        };

        Ok(Self {
            child: Some(child),
            pid,
            endpoints,
            ready_elapsed: spawn_start.elapsed(),
        })
    }

    pub fn endpoints(&self) -> &Endpoints {
        &self.endpoints
    }

    /// PID of the spawned sæhrimnir child. Stable across the lifetime of
    /// the handle (until `shutdown` consumes it).
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Wall-clock time from spawning sæhrimnir to parsing its readiness
    /// sentinel. Surfaced in the per-phase summary printed by `sync`
    /// so a slow cold-start is visible.
    pub fn ready_elapsed(&self) -> Duration {
        self.ready_elapsed
    }

    /// SIGTERM the child, grant [`SHUTDOWN_BUDGET`], escalate to SIGKILL
    /// on timeout. Always reaps. Consumes the handle so the Drop fallback
    /// can't double-fire.
    pub fn shutdown(mut self) -> Result<MockOutcome, DevError> {
        use std::os::unix::process::ExitStatusExt;

        let Some(mut child) = self.child.take() else {
            return Ok(MockOutcome {
                exit_code: None,
                signal: None,
                killed_after_budget: false,
                shutdown_elapsed: Duration::ZERO,
            });
        };
        let shutdown_start = Instant::now();
        kill_for_shutdown(self.pid, libc::SIGTERM);
        match wait_with_deadline(&mut child, self.pid, SHUTDOWN_BUDGET) {
            Ok(waited) => Ok(MockOutcome {
                exit_code: waited.status.code(),
                signal: waited.status.signal(),
                killed_after_budget: waited.killed_after_budget,
                shutdown_elapsed: shutdown_start.elapsed(),
            }),
            Err(error) => Err(error),
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
            kill_for_shutdown(self.pid, libc::SIGKILL);
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
/// inspection both have a known path. `sync` puts its sentinel
/// inside the per-run artefact dir instead.
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
    let paths = MockPaths::resolve(req.config, req.project_root)?;
    let fixture_path = paths.fixture(req.fixture)?;
    let binary = paths.binary;

    let _lock = crate::context::reenter_command_lock()?;

    let state = ProjectState::new(req.project_root);
    let mock_dir = state.ratatoskr().mock_artifacts();
    std::fs::create_dir_all(&mock_dir).map_err(DevError::Io)?;
    let readiness = mock_dir.join("readiness");
    // Stale sentinel from a prior crashed run would fool wait_for_endpoints
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

    // Install SIGINT + SIGTERM handlers BEFORE we spawn sæhrimnir so a
    // `brokkr kill` arriving during the readiness window can't default-
    // terminate brokkr and orphan the freshly-spawned child. Default
    // handlers would also kill brokkr immediately during the foreground
    // wait phase; intercepting lets us drain the child within
    // [`SHUTDOWN_BUDGET`].
    let _signals = MockServeSignalGuard::install();

    // Own process group so the SIGTERM/SIGKILL paths reach sæhrimnir's
    // helper listeners via `kill(-pgid, ...)`. Safe here because
    // MockServeSignalGuard (installed above) bridges terminal signals.
    use std::os::unix::process::CommandExt;
    let mut child = Command::new(&binary)
        .args([
            "--fixture",
            &fixture_str,
            "--readiness-file",
            &readiness_str,
        ])
        .stdin(Stdio::null())
        // Inherit stdout/stderr so the user sees sæhrimnir's logs live.
        // Capture-and-replay would obscure timing for a foreground tool.
        .process_group(0)
        .spawn()
        .map_err(|error| DevError::Spawn {
            program: binary.display().to_string(),
            error,
        })?;
    let child_pid = child.id();
    _lock.set_child_pid(child_pid);

    let endpoints = match wait_for_endpoints_signal_aware(&mut child, &readiness, READINESS_BUDGET)
    {
        Ok(ep) => ep,
        Err(e) => {
            // Bounded cleanup if sæhrimnir spawned but never wrote the
            // sentinel - don't leave it running, and don't let a wedged
            // child make `brokkr kill` hang here either. SIGTERM, give
            // SHUTDOWN_BUDGET, then SIGKILL via wait_with_deadline.
            kill_for_shutdown(child_pid, libc::SIGTERM);
            if let Err(cleanup_error) = wait_with_deadline(&mut child, child_pid, SHUTDOWN_BUDGET) {
                output::ratatoskr_msg(&format!(
                    "mock-server cleanup also failed after readiness error: {cleanup_error}"
                ));
            }
            return Err(e);
        }
    };

    print_endpoints(&endpoints);
    output::ratatoskr_msg("press Ctrl-C to stop");

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
    Ok(resolve_config_path(project_root, raw))
}

/// Build a list of `(env_var_name, value)` pairs for the protocols whose
/// `test_endpoint_env_<proto>` field is set in `[ratatoskr]`. Owned
/// strings so the caller can hand `&str` views into
/// `run_captured_with_env_and_deadline` without lifetime gymnastics.
///
/// URL shapes match what ratatoskr's existing client code expects:
/// HTTP origins for the JSON-over-HTTP protocols, `host:port` for the
/// stream protocols.
///
/// Shared by `sync` (where it originated) and the
/// fixture-aware `service` paths.
pub fn endpoint_env_pairs(cfg: &RatatoskrConfig, endpoints: &Endpoints) -> Vec<(String, String)> {
    Protocol::all()
        .filter_map(|protocol| {
            protocol
                .configured_env(cfg)
                .map(|name| (name.to_owned(), protocol.endpoint(endpoints.port(protocol))))
        })
        .collect()
}

/// Pick the right file for a fixture name. `.toml` and `.lua` are both
/// valid sæhrimnir fixture formats; the loader dispatches by extension.
/// If the name already carries one of those extensions, take it
/// literally - that's the disambiguation hatch when both
/// `<stem>.toml` and `<stem>.lua` coexist. Bare stems pick whichever
/// file exists, and refuse if both do (the user almost certainly has a
/// stale copy in the wrong format).
pub fn resolve_fixture(fixtures_dir: &Path, name: &str) -> Result<PathBuf, DevError> {
    validate_fixture_name(name)?;

    if name.ends_with(".toml") || name.ends_with(".lua") {
        let path = fixtures_dir.join(name);
        return if fixture_is_inside(fixtures_dir, &path)? {
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
    match (
        fixture_is_inside(fixtures_dir, &toml_path)?,
        fixture_is_inside(fixtures_dir, &lua_path)?,
    ) {
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

fn validate_fixture_name(name: &str) -> Result<(), DevError> {
    let mut components = Path::new(name).components();
    let single_normal_component = matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none()
        && !name.contains('/')
        && !name.contains('\\');
    if !single_normal_component {
        return Err(DevError::Config(format!(
            "invalid fixture name {name:?}: expected a single name inside the configured fixtures directory"
        )));
    }
    Ok(())
}

/// Existing fixture files must resolve below the configured directory.
/// This also rejects a one-component name whose file is a symlink out of
/// the fixture tree.
fn fixture_is_inside(fixtures_dir: &Path, candidate: &Path) -> Result<bool, DevError> {
    if !candidate.exists() {
        return Ok(false);
    }
    let root = fixtures_dir.canonicalize().map_err(DevError::Io)?;
    let resolved = candidate.canonicalize().map_err(DevError::Io)?;
    if !resolved.starts_with(&root) {
        return Err(DevError::Config(format!(
            "fixture path {} resolves outside the configured fixtures directory {}",
            candidate.display(),
            fixtures_dir.display()
        )));
    }
    Ok(true)
}

/// Poll for the readiness sentinel while keeping the child alive.
/// Returns the parsed [`Endpoints`] on success; errors if the child
/// exits before writing the sentinel (covers fixture-validation errors,
/// port-in-use, etc.) or if the budget expires.
///
/// Also polls the cooperative-shutdown flag, so a `brokkr kill` landing
/// while an orchestrator's `SigtermGuard` is active aborts the wait with
/// `DevError::Interrupted` (and [`MockServer::spawn_observed`] reaps the
/// child) instead of waiting out the readiness budget. Without a guard the
/// flag is never set, so this costs nothing there.
pub fn wait_for_endpoints(
    child: &mut std::process::Child,
    readiness: &Path,
    budget: Duration,
) -> Result<Endpoints, DevError> {
    wait_for_endpoints_inner(
        child,
        readiness,
        budget,
        &crate::shutdown::is_shutdown_requested,
    )
}

/// As [`wait_for_endpoints`], but also polls a caller-supplied signal
/// predicate every tick. Returning `true` from the predicate aborts the
/// wait with `DevError::Interrupted` so a cooperative shutdown reaches
/// the caller during the readiness window. Used by `mock-serve` to honor
/// `brokkr kill` arriving before sæhrimnir is ready.
pub fn wait_for_endpoints_signal_aware(
    child: &mut std::process::Child,
    readiness: &Path,
    budget: Duration,
) -> Result<Endpoints, DevError> {
    wait_for_endpoints_inner(child, readiness, budget, &|| {
        MOCK_SIGNAL_FLAG.load(Ordering::Relaxed)
    })
}

fn wait_for_endpoints_inner(
    child: &mut std::process::Child,
    readiness: &Path,
    budget: Duration,
    interrupted: &dyn Fn() -> bool,
) -> Result<Endpoints, DevError> {
    let started = Instant::now();
    loop {
        if interrupted() {
            return Err(DevError::Interrupted);
        }
        if let Some(status) = child.try_wait().map_err(DevError::Io)? {
            let stderr_path = readiness.with_file_name("stderr.log");
            return Err(DevError::Subprocess {
                program: "saehrimnir".into(),
                ending: ProcessEnding::from_status(&status),
                stderr: format!(
                    "exited before writing the readiness sentinel; see {}",
                    stderr_path.display()
                ),
            });
        }
        if readiness.exists() {
            let text = std::fs::read_to_string(readiness).map_err(DevError::Io)?;
            return parse_sentinel(&text);
        }
        if started.elapsed() > budget {
            return Err(DevError::Verify(format!(
                "saehrimnir did not write a readiness sentinel within {budget:?}."
            )));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Print the per-protocol endpoint table for a manual session, using
/// the same URL shape as the harness environment values.
fn print_endpoints(ep: &Endpoints) {
    output::ratatoskr_msg("listening on:");
    for protocol in Protocol::all() {
        output::ratatoskr_msg(&format!(
            "  {:<6} {}",
            protocol.name(),
            protocol.endpoint(ep.port(protocol))
        ));
    }
}

/// Send a signal to the managed child's process group. Every sæhrimnir
/// lifecycle creates one process group so helpers are stopped with it.
fn kill_for_shutdown(pid: u32, signum: i32) {
    crate::process::signal_process_group(pid, signum).ok();
}

/// Wait for the child while watching the SIGINT/SIGTERM flag. On signal
/// we send SIGTERM and grant [`SHUTDOWN_BUDGET`] before SIGKILL; on
/// child-exit we just reap.
fn wait_with_signals(
    child: &mut std::process::Child,
    pid: u32,
) -> Result<std::process::ExitStatus, DevError> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(error) => {
                kill_for_shutdown(pid, libc::SIGKILL);
                let _reaped = child.wait();
                return Err(DevError::Io(error));
            }
        }
        if MOCK_SIGNAL_FLAG.load(Ordering::Relaxed) {
            output::ratatoskr_msg("signal received, shutting saehrimnir down");
            kill_for_shutdown(pid, libc::SIGTERM);
            return wait_with_deadline(child, pid, SHUTDOWN_BUDGET).map(|waited| waited.status);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// After SIGTERM, give the child up to `budget` to exit; SIGKILL if it
/// outlives that. SIGKILL still requires a final `wait` to reap, which
/// we always perform so the caller never sees a zombie PID.
pub fn wait_with_deadline(
    child: &mut std::process::Child,
    pid: u32,
    budget: Duration,
) -> Result<DeadlineWait, DevError> {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Ok(DeadlineWait {
                    status,
                    killed_after_budget: false,
                });
            }
            Ok(None) => {}
            Err(error) => {
                kill_for_shutdown(pid, libc::SIGKILL);
                let _reaped = child.wait();
                return Err(DevError::Io(error));
            }
        }
        if started.elapsed() > budget {
            output::ratatoskr_msg(&format!(
                "saehrimnir did not exit within {budget:?}, escalating to SIGKILL"
            ));
            kill_for_shutdown(pid, libc::SIGKILL);
            return child
                .wait()
                .map(|status| DeadlineWait {
                    status,
                    killed_after_budget: true,
                })
                .map_err(DevError::Io);
        }
        std::thread::sleep(SHUTDOWN_POLL_INTERVAL);
    }
}

// ---------------------------------------------------------------------------
// Signal handling for mock-serve. Scoped via RAII guard so non-mock-serve
// code keeps its default SIGINT/SIGTERM behaviour. Separate atomic from
// `crate::shutdown`'s state machine so the global `brokkr kill` flow
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
        // Mirror `crate::shutdown::SigtermGuard::Drop`: also reset the
        // flag so a captured subprocess invoked AFTER this guard ends
        // doesn't see a sticky `true` from a SIGTERM that already fired.
        MOCK_SIGNAL_FLAG.store(false, Ordering::Relaxed);
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
        let text = "JMAP 12345\nIMAP 23456\nSMTP 34567\nGRAPH 45678\nGMAIL 56789\nCALDAV 60000\nCARDDAV 60500\nPEOPLE 61000\nGCAL 62000\n";
        let ep = parse_sentinel(text).unwrap();
        assert_eq!(
            ep,
            Endpoints::from_ordered([
                12345, 23456, 34567, 45678, 56789, 60000, 60500, 61000, 62000,
            ])
        );
    }

    #[test]
    fn tolerates_blank_lines_and_arbitrary_order() {
        let text =
            "\nGMAIL 5\n\nCALDAV 6\nGRAPH 4\nJMAP 1\nIMAP 2\nSMTP 3\nCARDDAV 9\nPEOPLE 7\nGCAL 8\n";
        let ep = parse_sentinel(text).unwrap();
        assert_eq!(ep.port(Protocol::Jmap), 1);
        assert_eq!(ep.port(Protocol::Imap), 2);
        assert_eq!(ep.port(Protocol::Smtp), 3);
        assert_eq!(ep.port(Protocol::Graph), 4);
        assert_eq!(ep.port(Protocol::Gmail), 5);
        assert_eq!(ep.port(Protocol::Caldav), 6);
        assert_eq!(ep.port(Protocol::Carddav), 9);
        assert_eq!(ep.port(Protocol::People), 7);
        assert_eq!(ep.port(Protocol::Gcal), 8);
    }

    #[test]
    fn missing_entry_errors_with_named_protocols() {
        let text = "JMAP 1\nIMAP 2\nGMAIL 3\n";
        let err = parse_sentinel(text).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("missing entries"), "got: {msg}");
        assert!(msg.contains("SMTP"), "got: {msg}");
        assert!(msg.contains("GRAPH"), "got: {msg}");
        assert!(msg.contains("CALDAV"), "got: {msg}");
        assert!(msg.contains("CARDDAV"), "got: {msg}");
        assert!(msg.contains("PEOPLE"), "got: {msg}");
        assert!(msg.contains("GCAL"), "got: {msg}");
    }

    #[test]
    fn duplicate_entry_errors() {
        let text = "JMAP 1\nIMAP 2\nSMTP 3\nGRAPH 4\nGMAIL 5\nCALDAV 6\nCARDDAV 9\nPEOPLE 7\nGCAL 8\nJMAP 9\n";
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

    /// A fresh scratch dir for one fixture-resolution test. `name` must be
    /// unique within this module - the allocator asserts it.
    fn fixture_tmpdir(name: &str) -> PathBuf {
        crate::test_scratch::scratch("mock-serve-fixture", name)
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
    fn resolve_fixture_rejects_names_outside_the_fixture_directory() {
        let dir = fixture_tmpdir("invalid_names");
        let absolute = dir
            .canonicalize()
            .unwrap()
            .join("outside.toml")
            .display()
            .to_string();
        for name in [
            "../outside",
            "nested/fixture",
            "..\\outside",
            absolute.as_str(),
        ] {
            let err = resolve_fixture(&dir, name).unwrap_err();
            assert!(
                err.to_string().contains("invalid fixture name"),
                "got: {err}"
            );
        }
    }

    #[test]
    fn resolve_fixture_rejects_symlinks_outside_the_fixture_directory() {
        let dir = fixture_tmpdir("symlink_escape");
        let outside = fixture_tmpdir("symlink_target").join("secret.toml");
        std::fs::write(&outside, "").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("external.toml")).unwrap();

        let err = resolve_fixture(&dir, "external.toml").unwrap_err();
        assert!(
            err.to_string()
                .contains("outside the configured fixtures directory"),
            "got: {err}"
        );
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
