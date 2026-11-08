//! `brokkr bench NAME` end to end: [`run`] over a fixture crate that cargo
//! builds inside the test, in a git repository of its own, every arm rule of
//! `docs/commands/bench.md` (Arms) met for real. The fixture's executable acts
//! on its first argument: it moves HEAD (`commit`), sends brokkr the stop
//! `brokkr kill` sends (`stop`), or prints it; a fixture given the build
//! script moves HEAD or changes the tree during the build itself, by the
//! features built. Runs go to this process's
//! scratch bench store, or to a loopback results server over a real store
//! that can request a stop while acknowledging a publication, or refuse one.
//!
//! Nothing here reaches a per-user store: the lock, the bench store and the
//! lease store are this process's scratch ones under `cfg(test)`, the build is
//! pinned to the fixture's own target (`test_scratch::cargo_fixture_target`),
//! and the fixture refuses to start under an environment that would point git
//! or cargo's build directory elsewhere.

#![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]

use std::io::Write as _;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use super::{BenchArgs, run};
use crate::bench::remote::tests::{identity_body, read_request};
use crate::bench::store::{self, wire};
use crate::error::DevError;
use crate::project::Detection;

const MAIN_RS: &str = r#"use std::process::Command;

unsafe extern "C" {
    safe fn kill(pid: i32, signal: i32) -> i32;
}

#[cfg(feature = "broken")]
compile_error!("the broken feature does not build");

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    #[cfg(feature = "profiled")]
    if let Some(path) = std::env::var_os("HOTPATH_OUTPUT_PATH") {
        std::fs::write(path, "{\"functions_timing\":{\"data\":[{\"name\":\"main\",\"calls\":1}]}}")
            .unwrap();
    }
    match mode.as_str() {
        "commit" => {
            let mut git = Command::new("git");
            for (name, _) in std::env::vars_os() {
                if name.to_string_lossy().starts_with("GIT_") {
                    git.env_remove(name);
                }
            }
            let moved = git
                .args(["-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null"])
                .args(["commit", "--allow-empty", "--quiet", "-m", "moved"])
                .status()
                .is_ok_and(|status| status.success());
            std::process::exit(if moved { 0 } else { 1 });
        }
        "stop" => {
            let parent = i32::try_from(std::os::unix::process::parent_id()).unwrap_or(0);
            if parent > 1 {
                kill(parent, 15);
            }
            std::thread::sleep(std::time::Duration::from_secs(10));
        }
        _ => println!("{mode}"),
    }
}
"#;

const CARGO_TOML: &str = r#"[package]
name = "probe"
version = "0.1.0"
edition = "2024"
publish = false

[features]
broken = []
moves = []
dirties = []
cleans = []
profiled = []
instrumented = ["profiled"]

[workspace]
"#;

/// The build script a fixture gets only when it asks for one: by the
/// features built, it moves HEAD (`moves`), leaves an untracked file in the
/// tree (`dirties`) or removes that file (`cleans`), all during the build.
const BUILD_RS: &str = r#"use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let root = std::env::var_os("CARGO_MANIFEST_DIR").unwrap();
    let root = Path::new(&root);
    if std::env::var_os("CARGO_FEATURE_MOVES").is_some() {
        let mut git = Command::new("git");
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("GIT_") {
                git.env_remove(name);
            }
        }
        let moved = git
            .current_dir(root)
            .args(["-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null"])
            .args(["commit", "--allow-empty", "--quiet", "-m", "moved in the build"])
            .status()
            .is_ok_and(|status| status.success());
        assert!(moved, "the build script could not move HEAD");
    }
    if std::env::var_os("CARGO_FEATURE_DIRTIES").is_some() {
        std::fs::write(root.join("dirt"), "left by the build\n").unwrap();
    }
    if std::env::var_os("CARGO_FEATURE_CLEANS").is_some() {
        std::fs::remove_file(root.join("dirt")).unwrap();
    }
}
"#;

/// Deeper than any ancestor's config, so neither the target nor the build
/// directory can come from one.
const CARGO_CONFIG: &str = "[build]\ntarget-dir = \"target\"\nbuild-dir = \"target\"\n";

const GITIGNORE: &str = "target\nCargo.lock\n.brokkr\n";

const BENCHES: &str = r#"
[bench.probe]
profile = "dev"
params.mode = { default = "plain" }
script = """
function bench(params)
    binary(params.mode):ok()
    if params.mode == "fail" then fail("asked to fail") end
    record.mode = params.mode
end"""

[bench.broken]
profile = "dev"
features = ["broken"]
params.mode = { default = "plain" }
script = """
function bench(params)
    binary(params.mode):ok()
end"""

[bench.moves]
profile = "dev"
features = ["moves"]
params.mode = { default = "plain" }
script = """
function bench(params)
    binary(params.mode):ok()
end"""

[bench.dirties]
profile = "dev"
features = ["dirties"]
params.mode = { default = "plain" }
script = """
function bench(params)
    binary(params.mode):ok()
end"""

[bench.cleans]
profile = "dev"
features = ["cleans"]
params.mode = { default = "plain" }
script = """
function bench(params)
    binary(params.mode):ok()
end"""

[bench.profiled]
profile = "dev"
hotpath_features = ["profiled"]
params.mode = { default = "plain" }
script = """
function bench(params)
    record.status = binary_with({hotpath = "main"}, params.mode):ok().hotpath_status
end"""

[bench.gated]
profile = "dev"
params.mode = { default = "plain" }
script = """
function preflight(params)
    if params.mode == "refused" or params.mode == "also" then
        local r = binary_with({program = "brokkr-no-such-program"}, "-v")
        fail("needs brokkr-no-such-program: " .. r.start_error)
    end
    binary_with({program = "sh"}, "-c", "true"):ok()
end
function bench(params)
    binary(params.mode):ok()
    record.mode = params.mode
end"""

[bench.leaky]
profile = "dev"
features = ["instrumented"]
hotpath_features = ["profiled"]
params.mode = { default = "plain" }
script = """
function bench(params)
    binary(params.mode):ok()
end"""
"#;

/// Environment that would point a git call of this test, brokkr's own
/// included, at another repository or index, or override its config.
const GIT_REDIRECTS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
];

/// A fixture project: its root, which is also its build root, and brokkr's
/// detection of it.
struct Fixture {
    root: PathBuf,
    detection: Detection,
}

impl Fixture {
    fn new(name: &str) -> Self {
        Self::with_files(name, &[])
    }

    /// As [`Fixture::new`], with `files` (relative path, content) committed
    /// beside the crate.
    fn with_files(name: &str, files: &[(&str, &str)]) -> Self {
        for var in GIT_REDIRECTS {
            assert!(
                std::env::var_os(var).is_none(),
                "{var} is set: the fixture's git calls, and brokkr's, would reach another repository"
            );
        }
        assert!(
            std::env::var_os("CARGO_BUILD_BUILD_DIR").is_none(),
            "CARGO_BUILD_BUILD_DIR is set: the fixture build would share that build directory"
        );
        let root = crate::test_scratch::cargo_fixture(name);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join(".cargo")).unwrap();
        std::fs::write(root.join("src/main.rs"), MAIN_RS).unwrap();
        std::fs::write(root.join("Cargo.toml"), CARGO_TOML).unwrap();
        std::fs::write(root.join(".cargo/config.toml"), CARGO_CONFIG).unwrap();
        std::fs::write(root.join(".gitignore"), GITIGNORE).unwrap();
        std::fs::write(
            root.join("brokkr.toml"),
            format!("project = \"e2e-{name}\"\n{BENCHES}"),
        )
        .unwrap();
        for (path, content) in files {
            let path = root.join(path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, content).unwrap();
        }
        git(&root, &["init", "--quiet"]);
        git(&root, &["config", "user.name", "brokkr test"]);
        git(&root, &["config", "user.email", "test@example.invalid"]);
        git(&root, &["add", "--all"]);
        git(&root, &["commit", "--quiet", "-m", "fixture"]);
        let detection = Detection::load(root.clone(), root.clone()).unwrap();
        Self { root, detection }
    }

    fn head(&self) -> String {
        git(&self.root, &["rev-parse", "HEAD"])
    }

    /// `brokkr bench --iterations N BENCH PARAMS...`, its console lines
    /// captured from the run log.
    fn bench(&self, log: &str, bench: &str, iterations: u32, params: &[&str]) -> Ran {
        self.bench_as(log, bench, iterations, false, params)
    }

    /// [`Fixture::bench`], with `--hotpath` when `hotpath` is set.
    fn bench_as(
        &self,
        log: &str,
        bench: &str,
        iterations: u32,
        hotpath: bool,
        params: &[&str],
    ) -> Ran {
        let console = Console::open(log);
        let args = BenchArgs {
            name: Some(bench.to_owned()),
            commit: None,
            iterations: std::num::NonZeroU32::new(iterations).unwrap(),
            hotpath,
            params: params.iter().map(|p| (*p).to_owned()).collect(),
        };
        let mut argv = vec!["brokkr".to_owned(), "bench".to_owned(), bench.to_owned()];
        argv.extend(args.params.iter().cloned());
        let invocation = crate::db::RunInvocation {
            argv,
            positional_tail: 0,
            arm_key: String::new(),
            arm_qualifier: None,
        };
        let result = run(&args, &self.detection, &invocation);
        assert!(
            !crate::shutdown::should_unwind(),
            "a stop outlived the invocation that caught it"
        );
        Ran {
            result,
            console: console.text(),
        }
    }

    /// This project's runs in the local store, by their `mode` key.
    fn stored(&self) -> Vec<store::RunDocument> {
        let path = crate::bench::evidence::store_path().unwrap();
        documents(
            &store::Store::open(&path).unwrap(),
            self.detection.project.name(),
        )
    }
}

fn documents(store: &store::Store, project: &str) -> Vec<store::RunDocument> {
    let filter = store::RunFilter {
        project: project.to_owned(),
        ..store::RunFilter::default()
    };
    let mut runs: Vec<store::RunDocument> = store
        .select(&filter)
        .unwrap()
        .iter()
        .map(|uuid| store.load(uuid).unwrap().unwrap())
        .collect();
    runs.sort_by_key(|run| (run.bench.clone(), mode(run).to_owned()));
    runs
}

fn mode(run: &store::RunDocument) -> &str {
    &run.params["mode"].values[0].key
}

fn git(root: &Path, args: &[&str]) -> String {
    let mut command = crate::process::spawn::ChildPolicy::plain_command("git");
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("GIT_") {
            command.env_remove(name);
        }
    }
    command
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .current_dir(root);
    let output = crate::output::child::command_output(&mut command).expect("git is available");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// What one invocation returned and printed.
struct Ran {
    result: Result<(), DevError>,
    console: String,
}

impl Ran {
    fn error(&self) -> String {
        match &self.result {
            Err(error) => error.to_string(),
            Ok(()) => panic!("the invocation succeeded:\n{}", self.console),
        }
    }

    fn says(&self, line: &str) {
        assert!(
            self.console.contains(line),
            "{line:?} not printed:\n{}",
            self.console
        );
    }
}

/// The run log teed to a file for one invocation, closed when dropped, so a
/// failed assertion never leaves the console redirected.
struct Console(PathBuf);

impl Console {
    fn open(name: &str) -> Self {
        let path = crate::test_scratch::scratch_path("bench_runner_e2e", &format!("{name}.log"));
        crate::output::open_run_log(std::fs::File::create(&path).unwrap());
        Self(path)
    }

    fn text(&self) -> String {
        std::fs::read_to_string(&self.0).unwrap()
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        crate::output::close_run_log();
    }
}

/// What the loopback server does with a publication.
#[derive(Clone, Copy)]
enum OnPublish {
    /// Store it, and before acknowledging the Nth, request a stop.
    StoreStoppingAt(usize),
    /// Refuse every one, finally: the run is not stored.
    Refuse,
}

/// A results server on a loopback port over a real store behind
/// `wire::serve`, stopped and joined when dropped.
struct Loopback {
    url: crate::config::ResultsServer,
    address: SocketAddr,
    store: PathBuf,
    publishes: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Loopback {
    fn serve(name: &str, on_publish: OnPublish) -> Self {
        let store = crate::test_scratch::scratch("bench_runner_e2e_server", name).join("bench.db");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let publishes = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (store, publishes, stop) =
                (store.clone(), Arc::clone(&publishes), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut served = crate::bench::evidence::open_store(&store).unwrap();
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(mut stream) = stream else { continue };
                    let timeout = Some(Duration::from_secs(10));
                    drop(stream.set_read_timeout(timeout));
                    drop(stream.set_write_timeout(timeout));
                    let Some(request) = read_request(&mut stream) else {
                        continue;
                    };
                    let (status, body) = if request.method == "GET" {
                        (200, identity_body(wire::PROTOCOL_VERSION))
                    } else {
                        let nth = publishes.fetch_add(1, Ordering::SeqCst) + 1;
                        match on_publish {
                            OnPublish::Refuse => (
                                400,
                                wire::Failure::new(wire::Kind::Refused, "scripted").encode(),
                            ),
                            OnPublish::StoreStoppingAt(at) => {
                                let reply = wire::serve(&mut served, &request.body);
                                if nth == at {
                                    crate::shutdown::request_shutdown();
                                }
                                (if reply.failure.is_none() { 200 } else { 400 }, reply.body)
                            }
                        }
                    };
                    respond(&mut stream, status, &body);
                }
            })
        };
        Self {
            url: crate::config::ResultsServer::parse(&format!("http://{address}")).unwrap(),
            address,
            store,
            publishes,
            stop,
            thread: Some(thread),
        }
    }

    fn stored(&self, project: &str) -> Vec<store::RunDocument> {
        documents(&store::Store::open(&self.store).unwrap(), project)
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wakes the accept the thread is blocked in, so it sees the flag.
        drop(TcpStream::connect(self.address));
        if let Some(thread) = self.thread.take() {
            drop(thread.join());
        }
    }
}

fn respond(stream: &mut TcpStream, status: u16, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status} Scripted\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    drop(
        stream
            .write_all(head.as_bytes())
            .and_then(|()| stream.write_all(body)),
    );
}

fn failed_reason(run: &store::RunDocument) -> &str {
    match &run.outcome {
        store::RunOutcome::Failed { reason } => reason,
        other => panic!("expected a failed run, got {other:?}"),
    }
}

/// A run of the fixture's HEAD that built: clean, its commit and subject,
/// the dev profile's `probe`.
fn built_at(run: &store::RunDocument, head: &str) {
    assert_eq!(run.commit, head);
    assert!(!run.dirty, "the fixture tree is clean");
    assert_eq!(run.subject.as_deref(), Some("fixture"));
    assert_eq!(run.iterations_requested, 2);
    assert_eq!(run.build.profile, "dev");
    assert_eq!(run.build.resolved.as_ref().unwrap().bin, "probe");
    assert!(run.build.artifact.is_some());
}

/// A completed run of two iterations, each one child given the run's mode,
/// exiting 0, and the mode recorded.
fn completed_twice(run: &store::RunDocument) {
    assert_eq!(run.outcome, store::RunOutcome::Completed, "{run:?}");
    assert_eq!(run.iterations.len(), 2);
    for iteration in &run.iterations {
        assert_eq!(iteration.children.len(), 1);
        let child = &iteration.children[0];
        assert_eq!(child.argv.last().map(String::as_str), Some(mode(run)));
        assert_eq!(child.ending, store::ChildEnding::Exited { code: 0 });
        assert_eq!(iteration.record.as_ref().unwrap()["mode"], mode(run));
    }
}

/// A failed iteration fails its arm and does not stop the rest; every arm is
/// stored on its own, and the command then fails naming the failed ones.
#[test]
fn every_arm_runs_and_is_stored_and_the_failed_ones_are_named() {
    let _serial = crate::test_scratch::process_global_lock();
    let fixture = Fixture::new("fan-out");
    let _hold = crate::lockfile::test_hold(
        &fixture.detection.project_root,
        &fixture.detection.build_root,
    );
    let head = fixture.head();

    let ran = fixture.bench("fan-out", "probe", 2, &["--mode", "a,fail,b"]);
    assert_eq!(ran.error(), "1 of 3 arms of bench probe failed: mode=fail");
    ran.says("arm 1/3: mode=a");
    ran.says("arm 3/3: mode=b");
    ran.says("bench probe (mode=fail) failed: iteration 1/2: ");
    assert!(!ran.console.contains("not run"), "{}", ran.console);

    let runs = fixture.stored();
    assert_eq!(
        runs.iter().map(mode).collect::<Vec<_>>(),
        ["a", "b", "fail"]
    );
    for run in &runs {
        built_at(run, &head);
    }
    for run in &runs[..2] {
        completed_twice(run);
    }
    let reason = failed_reason(&runs[2]);
    assert!(
        reason.starts_with("iteration 1/2: ") && reason.contains("asked to fail"),
        "{reason}"
    );
    assert_eq!(
        runs[2].iterations.len(),
        1,
        "the run stops at its first failure"
    );

    // One arm: the summary names the bench alone.
    let ran = fixture.bench("single", "probe", 1, &["--mode", "fail"]);
    assert_eq!(ran.error(), "bench probe failed");
    assert!(!ran.console.contains("arm 1/1"), "{}", ran.console);
}

/// `--hotpath` builds with the bench's hotpath features and stores each
/// marked child's report; the same bench under timing builds without them,
/// and its marked call, uninstrumented, writes nothing and fails nothing. A
/// timing build that resolves a hotpath feature through another feature is
/// refused, and `--hotpath` on a bench with no hotpath features is refused
/// before anything is stored.
#[test]
fn hotpath_runs_profile_with_their_features_and_timing_runs_stay_clean() {
    let _serial = crate::test_scratch::process_global_lock();
    let fixture = Fixture::new("hotpath");
    let _hold = crate::lockfile::test_hold(
        &fixture.detection.project_root,
        &fixture.detection.build_root,
    );
    let ran = fixture.bench_as("hotpath", "profiled", 2, true, &["--mode", "p"]);
    assert!(ran.result.is_ok(), "{}", ran.console);
    let ran = fixture.bench_as("timing", "profiled", 1, false, &["--mode", "t"]);
    assert!(ran.result.is_ok(), "{}", ran.console);
    let runs: Vec<_> = fixture
        .stored()
        .into_iter()
        .filter(|run| run.bench == "profiled")
        .collect();
    let [profiled, timing] = runs.as_slice() else {
        panic!("{runs:?}");
    };
    assert_eq!(profiled.measurement, store::Measurement::Hotpath);
    assert!(profiled.build.features.contains(&"profiled".to_owned()));
    for iteration in &profiled.iterations {
        assert_eq!(iteration.record.as_ref().unwrap()["status"], "captured");
        let capture = iteration.children[0].hotpath.as_ref().unwrap();
        assert_eq!((capture.stage.as_str(), capture.occurrence), ("main", 1));
        assert!(matches!(
            capture.outcome,
            store::CaptureOutcome::Captured { .. }
        ));
    }
    assert!(profiled.pairing_key().contains("\"hotpath\""));
    assert_eq!(timing.measurement, store::Measurement::Timing);
    assert!(!timing.build.features.contains(&"profiled".to_owned()));
    assert_eq!(
        timing.iterations[0].record.as_ref().unwrap()["status"],
        "missing"
    );
    assert!(timing.pairing_key().starts_with("[\"bench-pair-v1\""));

    let ran = fixture.bench("leaky", "leaky", 1, &[]);
    ran.error();
    ran.says("resolved hotpath feature \"profiled\"");

    let ran = fixture.bench_as("unprofilable", "probe", 1, true, &[]);
    assert!(
        ran.error().contains("sets no hotpath_features"),
        "{}",
        ran.error()
    );
}

/// A preflight runs for every arm before anything is built: one arm's
/// refusal refuses the invocation whole, every refusing arm named, nothing
/// built and nothing stored; when every arm passes, the arms run as ever.
#[test]
fn a_preflight_refusal_refuses_every_arm_before_any_build() {
    let _serial = crate::test_scratch::process_global_lock();
    let fixture = Fixture::new("preflight");
    let _hold = crate::lockfile::test_hold(
        &fixture.detection.project_root,
        &fixture.detection.build_root,
    );
    let ran = fixture.bench("refused", "gated", 1, &["--mode", "a,refused,also"]);
    let error = ran.error();
    for arm in ["mode=refused", "mode=also"] {
        let line = error
            .lines()
            .find(|l| l.contains(&format!("bench gated ({arm}): preflight refused: ")))
            .unwrap_or_else(|| panic!("{arm} is not named: {error}"));
        assert!(
            line.contains("brokkr.toml:65: needs brokkr-no-such-program: resolve: ")
                && line.contains("is not on PATH"),
            "{line}"
        );
    }
    assert!(
        !error.contains("(mode=a)"),
        "a passing arm is not named: {error}"
    );
    assert!(!ran.console.contains("cargo build"), "{}", ran.console);
    assert!(!ran.console.contains("arm 1/3"), "{}", ran.console);
    assert!(fixture.stored().is_empty(), "nothing is stored");

    let ran = fixture.bench("passed", "gated", 1, &["--mode", "a,b"]);
    assert!(ran.result.is_ok(), "{}", ran.console);
    let runs = fixture.stored();
    assert_eq!(runs.iter().map(mode).collect::<Vec<_>>(), ["a", "b"]);
    for run in &runs {
        completed_once(run);
    }
}

/// `require` loads from `bench_lib` in the preflight and in every iteration,
/// and the run's stored config holds every module asked for, with its
/// source, as the script's own is held: a changed helper shows when two runs
/// are compared.
#[test]
fn modules_from_bench_lib_are_required_and_stored_with_the_run() {
    let _serial = crate::test_scratch::process_global_lock();
    let config = "project = \"e2e-modules\"\n\
                  bench_lib = \"lib\"\n\
                  [bench.mods]\n\
                  profile = \"dev\"\n\
                  script = \"\"\"\n\
                  local gate = require('gate')\n\
                  function preflight() gate.check() end\n\
                  function bench()\n\
                  \x20   binary(require('io.word').word):ok()\n\
                  \x20   record.word = require('io.word').word\n\
                  end\"\"\"\n";
    let fixture = Fixture::with_files(
        "modules",
        &[
            ("brokkr.toml", config),
            ("lib/gate.lua", "local M = {}\nfunction M.check() binary_with({program = 'sh'}, '-c', 'true'):ok() end\nreturn M\n"),
            ("lib/io/word.lua", "return { word = 'plain' }\n"),
            ("lib/unused.lua", "return {}\n"),
        ],
    );
    let _hold = crate::lockfile::test_hold(
        &fixture.detection.project_root,
        &fixture.detection.build_root,
    );
    let ran = fixture.bench("modules", "mods", 1, &[]);
    assert!(ran.result.is_ok(), "{}", ran.console);
    let runs = fixture.stored();
    let [run] = runs.as_slice() else {
        panic!("{runs:?}");
    };
    assert_eq!(run.iterations[0].record.as_ref().unwrap()["word"], "plain");
    let stored: toml::Table = run.config.parse().unwrap();
    let modules = stored["modules"].as_table().unwrap();
    assert_eq!(
        modules.keys().cloned().collect::<Vec<_>>(),
        ["gate", "io.word"],
        "what was asked for, never the rest of the library"
    );
    assert_eq!(
        modules["io.word"]["source"].as_str(),
        Some("return { word = 'plain' }\n")
    );
    assert!(
        modules["gate"]["origin"]
            .as_str()
            .unwrap()
            .ends_with("lib/gate.lua")
    );
}

fn completed_once(run: &store::RunDocument) {
    assert_eq!(run.outcome, store::RunOutcome::Completed, "{run:?}");
    assert_eq!(run.iterations.len(), 1);
}

/// A build failure is an arm's failure like any other: every arm builds,
/// fails and is stored, and the command names them all.
#[test]
fn a_build_failure_fails_its_arm_and_the_rest_still_run() {
    let _serial = crate::test_scratch::process_global_lock();
    let fixture = Fixture::new("build-fails");
    let _hold = crate::lockfile::test_hold(
        &fixture.detection.project_root,
        &fixture.detection.build_root,
    );
    let ran = fixture.bench("broken", "broken", 1, &["--mode", "a,b"]);
    assert_eq!(
        ran.error(),
        "2 of 2 arms of bench broken failed: mode=a; mode=b"
    );
    ran.says("arm 2/2: mode=b");
    ran.says("bench broken (mode=b) failed: build: ");
    let broken: Vec<_> = fixture
        .stored()
        .into_iter()
        .filter(|run| run.bench == "broken")
        .collect();
    assert_eq!(broken.iter().map(mode).collect::<Vec<_>>(), ["a", "b"]);
    for run in &broken {
        assert!(failed_reason(run).starts_with("build: "), "{run:?}");
        assert!(run.build.resolved.is_none() && run.build.artifact.is_none());
        assert!(run.iterations.is_empty());
    }
}

/// Without `--commit`, an arm builds the live tree under the sha resolved for
/// the invocation, so HEAD moving off it ends the invocation once the arm in
/// flight is stored; the arms after it are named, never run and never stored.
/// Moved by a child, after the build: the arm measured its snapshot, built at
/// the recorded commit, so it completes.
#[test]
fn head_moving_off_the_invocations_commit_ends_it_before_the_next_arm() {
    let _serial = crate::test_scratch::process_global_lock();
    let fixture = Fixture::new("head-moves");
    let _hold = crate::lockfile::test_hold(
        &fixture.detection.project_root,
        &fixture.detection.build_root,
    );
    let before = fixture.head();

    let ran = fixture.bench("head-moves", "probe", 1, &["--mode", "commit,b,c"]);
    let after = fixture.head();
    assert_ne!(before, after, "the fixture's child moved HEAD");
    let error = ran.error();
    assert!(
        error.contains(&format!("HEAD moved from {before} to {after}")),
        "{error}"
    );
    ran.says("not run, so not stored: mode=b; mode=c");

    let runs = fixture.stored();
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(mode(&runs[0]), "commit");
    assert_eq!(runs[0].outcome, store::RunOutcome::Completed);
    assert_eq!(runs[0].commit, before);
}

/// HEAD moving during an arm's own build fails that arm: stored with what it
/// built and no iteration, under the commit it was meant to be, and the move
/// then ends the invocation before the next arm.
#[test]
fn head_moving_during_the_build_fails_the_arm() {
    let _serial = crate::test_scratch::process_global_lock();
    let fixture = Fixture::with_files("build-moves", &[("build.rs", BUILD_RS)]);
    let _hold = crate::lockfile::test_hold(
        &fixture.detection.project_root,
        &fixture.detection.build_root,
    );
    let before = fixture.head();

    let ran = fixture.bench("build-moves", "moves", 1, &["--mode", "a,b"]);
    let after = fixture.head();
    assert_ne!(before, after, "the build script moved HEAD");
    let moved = format!("HEAD moved from {before} to {after}");
    ran.says(&format!(
        "bench moves (mode=a) failed: {moved} during the build"
    ));
    let error = ran.error();
    assert!(
        error.contains(&format!("{moved} during the run")),
        "{error}"
    );
    ran.says("not run, so not stored: mode=b");

    let runs: Vec<_> = fixture
        .stored()
        .into_iter()
        .filter(|run| run.bench == "moves")
        .collect();
    assert_eq!(runs.len(), 1, "{runs:?}");
    let run = &runs[0];
    assert_eq!(mode(run), "a");
    assert_eq!(run.commit, before);
    assert!(
        failed_reason(run).starts_with(&format!("{moved} during the build")),
        "{run:?}"
    );
    assert!(run.build.resolved.is_some() && run.build.artifact.is_some());
    assert!(run.iterations.is_empty());
}

/// The live tree is read again once the build is done: a run is dirty if the
/// tree was before its build or is after it, and a dirty run still runs and
/// completes.
#[test]
fn a_tree_dirty_before_or_after_the_build_marks_the_run_dirty() {
    let _serial = crate::test_scratch::process_global_lock();
    let fixture = Fixture::with_files("build-dirt", &[("build.rs", BUILD_RS)]);
    let _hold = crate::lockfile::test_hold(
        &fixture.detection.project_root,
        &fixture.detection.build_root,
    );
    let head = fixture.head();
    let run_of = |bench: &str| {
        let runs: Vec<_> = fixture
            .stored()
            .into_iter()
            .filter(|run| run.bench == bench)
            .collect();
        assert_eq!(runs.len(), 1, "{runs:?}");
        let run = runs.into_iter().next().unwrap();
        assert_eq!(run.outcome, store::RunOutcome::Completed, "{run:?}");
        assert_eq!(run.commit, head);
        run
    };

    // Clean before, dirty after: the build left a file.
    let ran = fixture.bench("dirties", "dirties", 1, &[]);
    assert!(ran.result.is_ok(), "{:?}\n{}", ran.result, ran.console);
    assert!(fixture.root.join("dirt").exists());
    assert!(run_of("dirties").dirty);

    // Dirty before, clean after: the build removed it.
    let ran = fixture.bench("cleans", "cleans", 1, &[]);
    assert!(ran.result.is_ok(), "{:?}\n{}", ran.result, ran.console);
    assert!(!fixture.root.join("dirt").exists());
    assert!(run_of("cleans").dirty);

    // Clean on both sides.
    let ran = fixture.bench("clean", "probe", 1, &[]);
    assert!(ran.result.is_ok(), "{:?}\n{}", ran.result, ran.console);
    assert!(!run_of("probe").dirty);
}

/// A stop while a child runs (what `brokkr kill` sends, from the child
/// itself) stops the child, stores the arm interrupted and ends the
/// invocation with the rest named and not run.
#[test]
fn a_stop_inside_an_iteration_stores_its_arm_interrupted_and_ends_the_invocation() {
    let _serial = crate::test_scratch::process_global_lock();
    let fixture = Fixture::new("stopped");
    let _hold = crate::lockfile::test_hold(
        &fixture.detection.project_root,
        &fixture.detection.build_root,
    );

    let ran = fixture.bench("stopped", "probe", 1, &["--mode", "stop,b"]);
    assert!(
        matches!(ran.result, Err(DevError::Interrupted)),
        "{:?}\n{}",
        ran.result,
        ran.console
    );
    ran.says("not run, so not stored: mode=b");

    let runs = fixture.stored();
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(mode(&runs[0]), "stop");
    assert_eq!(runs[0].outcome, store::RunOutcome::Interrupted);
    let iteration = &runs[0].iterations[0];
    assert_eq!(iteration.outcome, store::IterationOutcome::Interrupted);
    assert_eq!(iteration.children.len(), 1);
    assert_eq!(
        iteration.children[0].ending,
        store::ChildEnding::Interrupted
    );
}

/// A stop that lands while an arm is being stored ends the invocation once
/// that arm is stored; one landing while the last arm is stored ends nothing.
#[test]
fn a_stop_while_an_arm_is_stored_ends_the_invocation_unless_it_was_the_last() {
    let _serial = crate::test_scratch::process_global_lock();
    let mut fixture = Fixture::new("stored-stop");
    let _hold = crate::lockfile::test_hold(
        &fixture.detection.project_root,
        &fixture.detection.build_root,
    );
    let project = fixture.detection.project.name().to_owned();

    let first = Loopback::serve("stop-at-first", OnPublish::StoreStoppingAt(1));
    fixture.detection.config.results_server = Some(first.url.clone());
    let ran = fixture.bench("stop-at-first", "probe", 1, &["--mode", "a,b,c"]);
    assert!(
        matches!(ran.result, Err(DevError::Interrupted)),
        "{:?}\n{}",
        ran.result,
        ran.console
    );
    ran.says("not run, so not stored: mode=b; mode=c");
    assert_eq!(first.publishes.load(Ordering::SeqCst), 1);
    let runs = first.stored(&project);
    assert_eq!(runs.iter().map(mode).collect::<Vec<_>>(), ["a"]);
    assert_eq!(runs[0].outcome, store::RunOutcome::Completed);
    drop(first);

    let last = Loopback::serve("stop-at-last", OnPublish::StoreStoppingAt(2));
    fixture.detection.config.results_server = Some(last.url.clone());
    let ran = fixture.bench("stop-at-last", "probe", 1, &["--mode", "a,b"]);
    assert!(ran.result.is_ok(), "{:?}\n{}", ran.result, ran.console);
    assert!(!ran.console.contains("not run"), "{}", ran.console);
    let runs = last.stored(&project);
    assert_eq!(runs.iter().map(mode).collect::<Vec<_>>(), ["a", "b"]);
    assert!(
        fixture.stored().is_empty(),
        "nothing went to the local store"
    );
}

/// A run that cannot be stored ends the invocation: a failed arm's cause is
/// printed, since it is stored nowhere, and the arms after it are named.
#[test]
fn a_run_that_cannot_be_stored_ends_the_invocation() {
    let _serial = crate::test_scratch::process_global_lock();
    let mut fixture = Fixture::new("unstored");
    let _hold = crate::lockfile::test_hold(
        &fixture.detection.project_root,
        &fixture.detection.build_root,
    );

    let server = Loopback::serve("refusing", OnPublish::Refuse);
    fixture.detection.config.results_server = Some(server.url.clone());
    let ran = fixture.bench("unstored", "probe", 1, &["--mode", "fail,b"]);
    let error = ran.error();
    assert!(error.contains("was not stored"), "{error}");
    ran.says("bench probe (mode=fail) failed: iteration 1/1: ");
    ran.says("not run, so not stored: mode=b");
    assert_eq!(server.publishes.load(Ordering::SeqCst), 1);
    assert!(server.stored(fixture.detection.project.name()).is_empty());
}
