//! Inputs and artifact identities carried by a prepared test plan, and the
//! barrier a cargo-launched run's harnesses wait on while its build is held
//! to that plan.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// xxh3-64 of a file's contents, hex, streamed: the identity a plan records
/// for an executable, and checks again before it runs.
pub(crate) fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    let mut buf = vec![0_u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:016x}", hasher.digest()))
}

/// What a strict session admits.
#[derive(Clone, Default, Debug)]
pub(crate) struct StrictPlan {
    /// Executable path -> content hash: the harnesses this run may attribute.
    /// A harness is admitted only when its path is here AND the file at that
    /// path still hashes to the planned value at its handshake - the last
    /// moment before it runs.
    pub(crate) known: HashMap<String, String>,
    /// Whether this run is a planned doctest carrier: only then may a rustdoc
    /// stream connect.
    pub(crate) rustdoc: bool,
    /// Set only on a carrier brokkr derived for a complete claim: every
    /// rustdoc must name its package directory, which must be here. A
    /// library whose manifest sets `doctest = false` is declined - its rustdoc
    /// exits successfully without running, and the decline is recorded -
    /// since an explicit `cargo test --doc` would test it regardless; one not
    /// here at all is refused. A configured `doc_only` lane holds `None`: its
    /// author asked for `--doc` and gets cargo's meaning of it.
    pub(crate) rustdoc_admission: Option<BTreeMap<PathBuf, LibraryDoctest>>,
}

/// One member library rustdoc can test, as a derived carrier admits it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LibraryDoctest {
    pub(crate) package: String,
    /// The manifest leaves `doctest` on.
    pub(crate) doctested: bool,
}

/// Where the build a cargo-launched run is about to execute stands against
/// the plan it runs under. The runner knows no plan: the caller compares the
/// run's own artifact stream (`build-finished` arrives before cargo spawns
/// its first test runner) and settles the gate; the harness shim holds every
/// connection until it is settled, so nothing executes - and nothing a test
/// does can touch the files being compared - before the comparison is made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GateState {
    /// No `build-finished` seen yet.
    Pending,
    /// `build-finished` seen; the comparison is running.
    Received,
    /// The build is the planned one.
    Clean,
    /// The build is not the planned one: one line per difference.
    Drift(Vec<String>),
    /// Cargo's build failed, so nothing was compared.
    BuildFailed,
    /// The comparison could not be made (a file could not be hashed).
    Failed(String),
    /// A connection waited past the bound with no build stream seen.
    Expired,
}

impl GateState {
    fn is_terminal(&self) -> bool {
        !matches!(self, Self::Pending | Self::Received)
    }

    /// Why a strict session refuses a connection on this state.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Pending | Self::Received => "the build has not been verified".into(),
            Self::Clean => "the build is the planned one".into(),
            Self::Drift(lines) => format!("the build cargo is about to run is not the planned one - {}", lines.join("; ")),
            Self::BuildFailed => "cargo's build failed".into(),
            Self::Failed(detail) => format!("the build could not be verified: {detail}"),
            Self::Expired => "the run's build stream was not seen before its first harness connected".into(),
        }
    }
}

/// How long a connection waits for cargo's `build-finished` record to be
/// read. Cargo writes it before spawning any runner, so the wait only covers
/// the stdout drain catching up; once the record is seen, the comparison is
/// waited for however long it takes (the phase watchdog bounds that).
pub(crate) const GATE_PENDING_BOUND: Duration = Duration::from_secs(30);

/// What runs the moment a wait expires the gate.
type ExpiryHook = Box<dyn FnOnce() + Send>;

/// The verification barrier one cargo run's connections wait on.
pub(crate) struct BuildGate {
    state: Mutex<GateState>,
    changed: Condvar,
    on_expiry: Mutex<Option<ExpiryHook>>,
}

impl Default for BuildGate {
    fn default() -> Self {
        Self { state: Mutex::new(GateState::Pending), changed: Condvar::new(), on_expiry: Mutex::new(None) }
    }
}

impl BuildGate {
    /// Run `hook` when a wait expires the gate - in the waiting thread,
    /// before that connection is answered, so whatever the caller records
    /// about an unverified build lands before anything it built runs.
    pub(crate) fn on_expiry(&self, hook: impl FnOnce() + Send + 'static) {
        *self.on_expiry.lock().unwrap_or_else(PoisonError::into_inner) = Some(Box::new(hook));
    }

    /// `build-finished` was seen; the comparison starts.
    pub(crate) fn receive(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if *state == GateState::Pending {
            *state = GateState::Received;
            self.changed.notify_all();
        }
    }

    /// Settle the gate, unless it already is: a terminal state is never
    /// overwritten, so a comparison finishing after a waiter gave up does not
    /// turn an expiry into a pass. Returns whether `to` took.
    pub(crate) fn settle(&self, to: GateState) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.is_terminal() {
            return false;
        }
        *state = to;
        self.changed.notify_all();
        true
    }

    pub(crate) fn state(&self) -> GateState {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Block until the gate is settled. While nothing has been received the
    /// wait is bounded by `pending_bound`, after which the gate settles as
    /// [`GateState::Expired`] (and the expiry hook runs). `closed` is checked
    /// first and throughout, a settled gate included; `None` when it says the
    /// wait is over (the session is closing, a stop was requested).
    pub(crate) fn wait(&self, pending_bound: Duration, closed: impl Fn() -> bool) -> Option<GateState> {
        let started = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if closed() {
                return None;
            }
            if state.is_terminal() {
                return Some(state.clone());
            }
            if *state == GateState::Pending && started.elapsed() >= pending_bound {
                *state = GateState::Expired;
                self.changed.notify_all();
                drop(state);
                let hook = self.on_expiry.lock().unwrap_or_else(PoisonError::into_inner).take();
                if let Some(hook) = hook {
                    hook();
                }
                return Some(GateState::Expired);
            }
            state = self
                .changed
                .wait_timeout(state, Duration::from_millis(50))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

/// A package directory as the admission table keys it: canonical where the
/// path resolves, as given otherwise - cargo's `CARGO_MANIFEST_DIR` and the
/// metadata's manifest path must meet on one spelling.
pub(crate) fn canonical_package_dir(dir: &Path) -> PathBuf {
    fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())
}

#[cfg(test)]
mod gate_tests {
    use super::{BuildGate, GateState};
    use std::sync::Arc;
    use std::time::Duration;

    /// A waiter is released by the comparison's verdict, whichever it is.
    #[test]
    fn a_waiter_is_released_by_the_verdict() {
        let gate = Arc::new(BuildGate::default());
        let settler = Arc::clone(&gate);
        let handle = std::thread::spawn(move || {
            settler.receive();
            std::thread::sleep(Duration::from_millis(20));
            settler.settle(GateState::Drift(vec!["x was rebuilt".into()]))
        });
        let seen = gate.wait(Duration::from_secs(5), || false);
        assert!(handle.join().expect("settler"));
        assert_eq!(seen, Some(GateState::Drift(vec!["x was rebuilt".into()])));
    }

    /// With no build stream received the wait is bounded, and the expiry is
    /// final: a comparison finishing late cannot turn it into a pass.
    #[test]
    fn an_expiry_is_never_overwritten() {
        let gate = BuildGate::default();
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook = Arc::clone(&fired);
        gate.on_expiry(move || hook.store(true, std::sync::atomic::Ordering::Relaxed));
        assert_eq!(gate.wait(Duration::ZERO, || false), Some(GateState::Expired));
        assert!(fired.load(std::sync::atomic::Ordering::Relaxed), "the expiry hook runs before the waiter is answered");
        assert!(!gate.settle(GateState::Clean));
        assert_eq!(gate.state(), GateState::Expired);
    }

    /// A stop or a closing session ends the wait even on a settled gate.
    #[test]
    fn a_closed_session_is_not_admitted_on_a_settled_gate() {
        let gate = BuildGate::default();
        assert!(gate.settle(GateState::Clean));
        assert_eq!(gate.wait(Duration::from_secs(5), || true), None);
    }

    /// Once the stream is received the comparison is waited for past the
    /// pending bound; a closing session ends the wait with no verdict.
    #[test]
    fn a_received_gate_outlives_the_pending_bound_until_closed() {
        let gate = BuildGate::default();
        gate.receive();
        let polls = std::sync::atomic::AtomicUsize::new(0);
        let seen = gate.wait(Duration::ZERO, || polls.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 3);
        assert_eq!(seen, None);
        assert_eq!(gate.state(), GateState::Received);
    }
}
