//! `brokkr bench NAME` end to end: [`run`] over a fixture crate that cargo
//! builds inside the test, in a git repository of its own, every arm rule of
//! `docs/commands/bench.md` (Arms) met for real. The fixture's executable acts
//! on its first argument: it moves HEAD (`commit`), sends brokkr the stop
//! `brokkr kill` sends (`stop`), or prints it. Runs go to this process's
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

[workspace]
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
        let console = Console::open(log);
        let args = BenchArgs {
            name: Some(bench.to_owned()),
            commit: None,
            iterations: std::num::NonZeroU32::new(iterations).unwrap(),
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
